//! A single-producer, single-consumer (SPSC) lock-free queue.

use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    ops::Range,
    ptr, slice,
    sync::atomic::{AtomicU32, Ordering, fence},
};

/// A single-producer, single-consumer (SPSC) lock-free queue storing up to `len-1` bytes.
/// `len` is defined by the leftover size of the region after the [`RingBuffer`] has taken its
/// size.
///
/// # ECC Padding
///
/// On MCUs with 64-bit ECC-protected RAM, each write only commits to memory when the
/// full 64-bit ECC word is written. If `read` and `write` share an ECC word, a reset
/// mid-write could corrupt both fields. Enable the `ecc-64bit` feature to add padding
/// that ensures each field occupies its own ECC word.
///
/// Note: The struct layout changes with this feature, so the MAGIC value differs to
/// force reinitialization when switching between configurations.
///
/// Note: Data writes don't need explicit ECC flushes. STM32H7/H5 MCUs use a single-word
/// ECC write cache - when we subsequently update the index (a different 64-bit word),
/// the cached data write is automatically flushed. The index itself needs the padding
/// write to flush its own 64-bit word before a potential reset.
#[repr(C)]
pub struct RingBuffer {
    /// If the value is [`MAGIC`], the struct is initialized.
    ///
    /// In particular, this means that the reader-owned part of the buffer
    /// contains real data.
    header: u128,
    /// Where the next read starts.
    ///
    /// The RingBuffer always guarantees `read < len`.
    read: AtomicU32,
    /// Padding to ensure `read` occupies its own 64-bit ECC word.
    /// Written after `read` to flush the ECC write buffer.
    #[cfg(feature = "ecc-64bit")]
    _pad_read: AtomicU32,
    /// Where the next write starts.
    ///
    /// The RingBuffer always guarantees `write < len`.
    write: AtomicU32,
    /// Padding to ensure `write` occupies its own 64-bit ECC word.
    /// Written after `write` to flush the ECC write buffer.
    #[cfg(feature = "ecc-64bit")]
    _pad_write: AtomicU32,
}

/// Writes data into the buffer.
pub struct Producer<'a> {
    header: &'a RingBuffer,
    buf: &'a [UnsafeCell<MaybeUninit<u8>>],
}

/// Reads data previously written to the buffer.
///
/// Returned by [`crate::init`]. Use [`Consumer::read`] to get a [`GrantR`] for accessing
/// the buffered data, then call [`GrantR::release`] to mark bytes as consumed.
///
/// With the `async-await` feature, use [`Consumer::wait_for_data`] to asynchronously
/// wait for new data to be available.
pub struct Consumer<'a> {
    header: &'a RingBuffer,
    buf: &'a [UnsafeCell<MaybeUninit<u8>>],
}

// SAFETY: Consumer can be safely sent to another thread because:
// - Only one Consumer exists per queue (single-consumer invariant enforced by split())
// - Atomic operations on header.read/write synchronize with the Producer
// - The UnsafeCell slice is only accessed through methods that maintain the SPSC invariant
unsafe impl Send for Consumer<'_> {}

/// Value used to indicate that the queue is initialized.
///
/// Replace this if the layout or field semantics change in a backwards-incompatible way.
/// The ECC-padded layout uses a different magic to force reinitialization when switching.
#[cfg(not(feature = "ecc-64bit"))]
const MAGIC: u128 = 0xb528_c25f_90c6_16af_cbc1_502c_09c1_fd6e;
#[cfg(feature = "ecc-64bit")]
const MAGIC: u128 = 0x1dff_2060_27b9_f2b4_a194_1013_69cd_3c6c;

/// Field offsets for corruption testing.
#[cfg(feature = "qemu-test")]
pub mod offsets {
    use super::RingBuffer;
    use core::mem::{offset_of, size_of};
    use core::sync::atomic::AtomicU32;

    /// Offset of the header field.
    pub const HEADER: usize = offset_of!(RingBuffer, header);
    /// Offset of the read index field.
    pub const READ: usize = offset_of!(RingBuffer, read);
    /// Offset of the write index field.
    pub const WRITE: usize = offset_of!(RingBuffer, write);
    /// Size of an index field.
    pub const INDEX_SIZE: usize = size_of::<AtomicU32>();
}

impl RingBuffer {
    #[cfg(test)]
    pub(crate) fn new(read: u32, write: u32) -> Self {
        RingBuffer {
            header: MAGIC,
            read: AtomicU32::new(read),
            write: AtomicU32::new(write),
            #[cfg(feature = "ecc-64bit")]
            _pad_read: AtomicU32::new(0),
            #[cfg(feature = "ecc-64bit")]
            _pad_write: AtomicU32::new(0),
        }
    }
    /// Creates a `RingBuffer` or recovers previous state if available.
    ///
    /// # Safety
    ///
    /// - `memory.start` must be aligned to `align_of::<RingBuffer>()`.
    /// - `memory.len()` must be greater than `size_of::<RingBuffer>()`.
    /// - Buffer size (`memory.len() - size_of::<RingBuffer>()`) must be less than
    ///   `i32::MAX / 4` to avoid overflow in pointer arithmetic.
    /// - With the `ecc-64bit` feature: both `memory.start` and `memory.end` must be
    ///   8-byte aligned for the 64-bit volatile writes that flush the ECC cache.
    /// - This takes logical ownership of the provided `memory` for the
    ///   `'static` lifetime. Make sure that any previous owner is no longer
    ///   live, for example by only ever having one application running at a
    ///   time and only one call to this function in the application's lifetime.
    ///
    /// It is, however, not a problem for both a bootloader and its booted
    /// application to call this function, provided the bootloader program
    /// ends when it boots into the application and cannot resume execution
    /// afterwards.
    ///
    /// There is always a risk that corrupt memory is accepted as
    /// valid. While this function checks for direct memory safety problems,
    /// it cannot vet the data in a non-empty buffer. Treat it as external
    /// input and do not rely on its value for memory safety.
    pub(crate) unsafe fn recover_or_reinitialize(
        memory: Range<usize>,
    ) -> (Producer<'static>, Consumer<'static>) {
        let v: *mut Self = ptr::with_exposed_provenance_mut(memory.start);
        let buf_len = memory.len() - size_of::<RingBuffer>();

        // SAFETY:
        // - Alignment is guaranteed by the caller.
        // - Size is guaranteed by the caller.
        // - All fields (`u128`, `AtomicU32`, `[UnsafeCell<MaybeUninit<u8>>, X]`)
        //   are valid for any bit pattern, so interpreting the raw memory as this
        //   type and buffer is sound. As the memory is initialized outside the Rust abstract
        //   machine (of the running program), we consider the caveats of non-fixed
        //   bit patterns from `MaybeUninit` mitigated.
        // - The caller guarantees this function is called at most once during
        //   program execution for any given `memory`, ensuring no aliasing
        //   references exist for the `'static` lifetime.
        let v = unsafe { &mut *v };
        let header = ptr::from_mut(&mut v.header);
        // SAFETY: A regular read from v.header would be safe here, but it would maybe be
        // optimizsed away.
        if unsafe { header.read_volatile() } != MAGIC {
            v.read.store(0, Ordering::Relaxed);
            #[cfg(feature = "ecc-64bit")]
            // SAFETY: Pointer is valid and aligned, from our own field.
            unsafe {
                v._pad_read.as_ptr().write_volatile(0)
            };
            // The intermediate state doesn't matter until header == MAGIC
            v.write.store(0, Ordering::Relaxed);
            #[cfg(feature = "ecc-64bit")]
            // SAFETY: Pointer is valid and aligned, from our own field.
            unsafe {
                v._pad_write.as_ptr().write_volatile(0)
            };

            fence(Ordering::SeqCst);
            // SAFETY: A regular assignment to v.header would be safe
            // here, but is not guaranteed to actually update memory. This
            // must mean the pointer is valid for writes and properly
            // aligned.
            unsafe { header.write_volatile(MAGIC) };
        } else {
            // The header promised to keep the contract, but we don't
            // trust it for the safety of our pointer offsets.
            let write = v.write.load(Ordering::Relaxed) as usize;
            let read = v.read.load(Ordering::Relaxed) as usize;
            let read_ok = read < buf_len;
            let write_ok = write < buf_len;
            // Since `header` is already marked as valid, some extra care
            // is taken here to avoid situations where there is a gap of time
            // where both indexes are in-bounds, but not valid. Otherwise
            // a poorly timed reset could leave the queue in a state that
            // appears valid and non-empty.
            match (read_ok, write_ok) {
                (true, true) => {}
                (true, false) => v.write.store(read as u32, Ordering::Relaxed),
                (false, true) => v.read.store(write as u32, Ordering::Relaxed),
                (false, false) => {
                    v.read.store(0, Ordering::Relaxed);
                    // write is still invalid between these operations
                    v.write.store(0, Ordering::Relaxed);
                }
            };
            #[cfg(feature = "ecc-64bit")]
            // SAFETY: Pointer is valid and aligned, from our own field.
            unsafe {
                v._pad_read.as_ptr().write_volatile(0)
            };
            #[cfg(feature = "ecc-64bit")]
            // SAFETY: Pointer is valid and aligned, from our own field.
            unsafe {
                v._pad_write.as_ptr().write_volatile(0)
            };
        }
        fence(Ordering::SeqCst);

        // SAFETY:
        // - The caller guarantees at least 1 byte of space left in the allocated area.
        // - There are no alignment requirements on the values in the buffer.
        // - There is no mutable aliasing, all slices made from this buffer are immutable - writes
        //   are made via the `UnsafeCell`s interior mutability.
        let buf: &[UnsafeCell<MaybeUninit<u8>>] = unsafe {
            slice::from_raw_parts(
                ptr::with_exposed_provenance(memory.start + size_of::<RingBuffer>()),
                buf_len,
            )
        };

        // SAFETY:
        // - The caller guarantees `buf.len() < i32::MAX / 4`.
        // - With `ecc-64bit`: the caller guarantees `memory.start` and `memory.end` are 8-byte aligned.
        //   Since `size_of::<RingBuffer>()` is a multiple of 8, `buf` inherits this alignment.
        unsafe { v.split(buf) }
    }

    /// Splits the queue into producer and consumer given a memory area.
    ///
    /// # Safety
    ///
    /// - `buf.len()` must be less than `i32::MAX / 4` to avoid overflow in pointer arithmetic.
    /// - With the `ecc-64bit` feature: `buf` must be 8-byte aligned at both start and end
    ///   (i.e., `buf.as_ptr()` and `buf.as_ptr().add(buf.len())` must be 8-byte aligned).
    ///   This is required for the 64-bit volatile writes that flush the ECC cache.
    /// - With the `ecc-64bit` feature: `buf` must not be backed by local `MaybeUninit` memory,
    ///   as the 64-bit read in the ECC flush could read uninitialized bytes that the compiler
    ///   can reason about (UB). Use linker-provided memory or memory with a defined bit pattern.
    #[inline]
    pub const unsafe fn split<'a>(
        &'a mut self,
        buf: &'a [UnsafeCell<MaybeUninit<u8>>],
    ) -> (Producer<'a>, Consumer<'a>) {
        (
            Producer { header: self, buf },
            Consumer { header: self, buf },
        )
    }
}

impl Producer<'_> {
    /// How much space is left in the buffer?
    #[inline]
    fn available(&self, read: usize, write: usize) -> usize {
        if read > write {
            read - write - 1
        } else {
            self.buf.len() - write - 1 + read
        }
    }

    /// Appends `data` to the buffer.
    ///
    /// If there is not enough space, the last bytes are silently discarded.
    #[inline]
    pub fn write(&mut self, data: &[u8]) {
        // Relaxed: stale `read` is safe (underestimates available space).
        let read = self.header.read.load(Ordering::Relaxed) as usize;
        // Relaxed: producer owns `write`, no cross-thread synchronization needed.
        let write = self.header.write.load(Ordering::Relaxed) as usize;
        let buf: *mut u8 = self.buf.as_ptr().cast_mut().cast();
        let len = data.len().min(self.available(read, write));
        if len == 0 {
            return;
        }

        // There are `ptr::copy_nonoverlapping` and `pointer::add` calls below.
        // The common safety arguments are:
        //
        // For `copy_nonoverlapping`:
        // - src valid: sub-slice of `data`, which is valid for reads.
        // - dst valid: sub-slice of the producer-owned part of `buf`, which is valid for writes.
        // - aligned: u8 slices have alignment 1.
        // - nonoverlapping: The caller-provided `data` cannot overlap with the part of `buf` owned
        //   by the producer, because only the consumer gives slices to external code.
        //
        // For `pointer::add`:
        // - offset in bytes fits in `isize`: the only constructor `RingBuffer::split`
        //   passes this requirement on to its caller.
        // - entire memory range inside the same allocation: we stay within `buf`, which is a
        //   single allocation.
        //
        // What remains to show for each use is that src and dst ranges are valid sub-slices of
        // `data` and the producer-owned part of `buf`, respectively.

        if write + len > self.buf.len() {
            // Wrapping case: the write crosses the end of the buffer.
            // This can only happen when write >= read (if write < read, then
            // available = read - write - 1, and
            // write + len <= write + read - write - 1 = read - 1 < buf.len(), = contradiction).
            let pivot = self.buf.len() - write;
            // SAFETY:
            // - First copy: data[0..pivot] -> buf[write..buf.len()]
            //   - src: pivot < len <= data.len() (since write + len > buf.len()
            //     implies len > buf.len() - write = pivot).
            //   - dst: write < buf.len() by field invariant, and
            //     write + pivot = buf.len(), so dst is buf[write..buf.len()].
            unsafe { ptr::copy_nonoverlapping(data.as_ptr(), buf.add(write), pivot) };
            // SAFETY:
            // - Second copy: data[pivot..len] -> buf[0..len-pivot]
            //   - src: pivot..len is in bounds since pivot < len <= data.len().
            //   - dst: len - pivot <= available - pivot. With write >= read,
            //     available = buf.len() - write - 1 + read, so
            //     len - pivot <= buf.len() - write - 1 + read - (buf.len() - write)
            //     = read - 1 < read. Thus buf[0..len-pivot] does not overlap
            //     with consumer-owned memory starting at read.
            unsafe { ptr::copy_nonoverlapping(data.as_ptr().add(pivot), buf, len - pivot) };
        } else {
            // Non-wrapping case: the entire write fits before the end.
            // SAFETY:
            // - src: data[0..len] is valid since len <= data.len().
            // - dst: buf[write..write+len]. write < buf.len() by field
            //   invariant, and write + len <= buf.len() by the else branch
            //   condition. len <= available ensures we don't write into
            //   consumer-owned memory.
            unsafe { ptr::copy_nonoverlapping(data.as_ptr(), buf.add(write), len) };
        }

        let new_write = write.wrapping_add(len) % self.buf.len();

        #[cfg(feature = "ecc-64bit")]
        // Flush ECC cache for the 8-byte block containing the last written byte.
        // This ensures data is committed to memory before the index update.
        //
        // SAFETY:
        // - We just wrote to this address, so it's valid for access.
        // - The contract of `RingBuffer::split` ensure the buffer is 8-byte aligned at both
        //   start and end, so the aligned 64-bit access stays within the allocated region.
        // - This does not cause data races with the `Consumer`: even if the aligned read-write
        //   touches bytes owned by the Consumer, we only write back the same value we read,
        //   and the Consumer never modifies those bytes, so no read-modify-write hazard exists.
        unsafe {
            let last_byte_pos = ((new_write + self.buf.len() - 1) % self.buf.len()) & !0x7;
            let aligned_addr = buf.add(last_byte_pos) as *mut u64;
            let val = aligned_addr.read();
            aligned_addr.write_volatile(val);
        }

        self.header.write.store(new_write as u32, Ordering::Release);
        #[cfg(feature = "ecc-64bit")]
        // SAFETY: Pointer is valid and aligned, from our own field.
        unsafe {
            self.header._pad_write.as_ptr().write_volatile(0)
        };
    }
}

impl Consumer<'_> {
    /// Returns `true` if there is no data available to read.
    #[inline]
    pub fn is_empty(&self) -> bool {
        // Acquire: synchronizes with producer's Release store to see written data.
        let write = self.header.write.load(Ordering::Acquire) as usize;
        // Relaxed: consumer owns `read`, no cross-thread synchronization needed.
        let read = self.header.read.load(Ordering::Relaxed) as usize;

        write == read
    }

    /// Read data from the buffer.
    ///
    /// If the data available to read crosses the end of the ring, this
    /// function may provide a smaller slice. Only after releasing the data
    /// up to the end of the ring will the next call provide more data.
    #[inline]
    #[must_use]
    pub fn read(&mut self) -> GrantR<'_, '_> {
        // Acquire: synchronizes with producer's Release store, ensuring we see the written data.
        let write = self.header.write.load(Ordering::Acquire) as usize;
        // Relaxed: consumer owns `read`, no cross-thread synchronization needed.
        let read = self.header.read.load(Ordering::Relaxed) as usize;
        let buf: *mut u8 = self.buf.as_ptr().cast_mut().cast();

        let (len1, len2) = if write < read {
            (self.buf.len() - read, write)
        } else {
            (write - read, 0)
        };

        // SAFETY:
        // For `slice::from_raw_parts`:
        // - Non-null, valid, aligned: it is a sub-slice of `buf`,
        //   relying on the invariants on `read` and `write`.
        // - Properly initialized values: The memory owned by the consumer
        //   has been initialized by the producer. When recovering the data
        //   from a previous run, we instead rely on the ability of u8 to
        //   accept any (fixed) bit pattern. Since the recovery procedure
        //   produces the value from memory outside the Rust abstract machine,
        //   the hazards of uninitialized memory should be mitigated.
        // - Not mutated for the lifetime: only the producer modifies
        //   `buf`, but the consumer owns this memory until the read pointer
        //   is updated. The read pointer is only updated in the function
        //   that drops the slice.
        // - Total size in bytes < i32::MAX: we stay inside `buf`
        //   and the only constructor `RingBuffer::split` requires of its caller
        //   that no in-bounds buffer is too big.
        //
        // For `pointer::add`:
        // - offset in bytes fits in `isize`: buf.len() fits, which is checked
        //   before constructing a Consumer. write - read fits if write >= read,
        //   which holds in the cases we use it.
        // - entire memory range inside the same allocation: read < len, so the
        //   offset remains in the buffer's allocation.
        let slice1 = unsafe { slice::from_raw_parts(buf.add(read), len1) };
        // SAFETY:
        // For `slice::from_raw_parts`:
        // - Non-null, valid, aligned: it is a sub-slice of `buf`,
        //   relying on the invariants on `read` and `write`.
        // - Properly initialized values: The memory owned by the consumer
        //   has been initialized by the producer. When recovering the data
        //   from a previous run, we instead rely on the ability of u8 to
        //   accept any (fixed) bit pattern. Since the recovery procedure
        //   produces the value from memory outside the Rust abstract machine,
        //   the hazards of uninitialized memory should be mitigated.
        // - Not mutated for the lifetime: only the producer modifies
        //   `buf`, but the consumer owns this memory until the read pointer
        //   is updated. The read pointer is only updated in the function
        //   that drops the slice.
        // - Total size in bytes < i32::MAX: we stay inside `buf`
        //   and the only constructor `RingBuffer::split` requires of its caller
        //   that no in-bounds buffer is too big.
        //
        // For `pointer::add`:
        // - offset in bytes fits in `isize`: buf.len() fits, which is checked
        //   before constructing a Consumer. write - read fits if write >= read,
        //   which holds in the cases we use it.
        // - entire memory range inside the same allocation: read < len, so the
        //   offset remains in the buffer's allocation.
        let slice2 = unsafe { slice::from_raw_parts(buf, len2) };
        GrantR {
            consumer: self,
            slice1,
            slice2,
            original_read: read,
        }
    }

    #[cfg(feature = "async-await")]
    /// Waits until there is data in the [`Consumer`].
    pub async fn wait_for_data(&mut self) {
        core::future::poll_fn(|cx| {
            super::logger::WAKER.register(cx.waker());

            if self.is_empty() {
                core::task::Poll::Pending
            } else {
                core::task::Poll::Ready(())
            }
        })
        .await
    }
}

/// A read grant providing access to buffered data.
///
/// Obtained from [`Consumer::read`]. The grant provides a slice of available data
/// via [`GrantR::buf`]. When done reading, call [`GrantR::release`] to mark bytes
/// as consumed and free space for new writes.
///
/// If the grant is dropped without calling `release`, no data is consumed.
pub struct GrantR<'a, 'c> {
    consumer: &'a Consumer<'c>,
    slice1: &'a [u8],
    slice2: &'a [u8],
    original_read: usize,
}

// SAFETY: GrantR can be safely sent to another thread because:
// - Only one GrantR can exist at a time (Consumer::read takes &mut self)
// - The slice is a regular &[u8] pointing to consumer-owned memory that the producer
//   won't modify until release() updates the read pointer
// - release() only performs atomic stores to header.read (and _pad_read for ECC)
// - The underlying UnsafeCell in Consumer::buf is not directly accessed through GrantR;
//   the slice was materialized in Consumer::read before GrantR was created
unsafe impl Send for GrantR<'_, '_> {}

impl<'a, 'c> GrantR<'a, 'c> {
    /// Finish the read, marking `used` elements as used
    ///
    /// This frees up the `used` space for future writes.
    #[inline]
    pub fn release(self, used: usize) {
        let used = used.min(self.slice1.len() + self.slice2.len());
        // Non-atomic read-modify-write is ok here because there can
        // never be more than one active GrantR at a time.
        let read = self.original_read;
        let new_read = if read + used < self.consumer.buf.len() {
            read + used
        } else {
            used - self.slice1.len()
        };
        self.consumer
            .header
            .read
            .store(new_read as u32, Ordering::Release);
        #[cfg(feature = "ecc-64bit")]
        // SAFETY: Pointer is valid and aligned, from our own field.
        unsafe {
            self.consumer.header._pad_read.as_ptr().write_volatile(0)
        };
    }

    /// Finish the read, marking all bytes as used.
    ///
    /// This is equivalent to `grant.release(grant.buf().len())`.
    #[inline]
    pub fn release_all(self) {
        self.release(usize::MAX);
    }

    /// Returns the bytes that this grant is allowed to read.
    #[inline]
    pub fn bufs(&self) -> (&[u8], &[u8]) {
        (self.slice1, self.slice2)
    }
}

#[cfg(test)]
mod test {

    use super::*;

    /// Buffer size for tests. Must be at least 8 when `ecc-64bit` is enabled for proper alignment.
    #[cfg(feature = "ecc-64bit")]
    const BUF_SIZE: usize = 16;
    #[cfg(not(feature = "ecc-64bit"))]
    const BUF_SIZE: usize = 4;

    /// 8-byte aligned buffer wrapper for tests.
    ///
    /// With `ecc-64bit`: `#[repr(align(8))]` ensures start is 8-byte aligned, and
    /// `BUF_SIZE` (16) is a multiple of 8, so end is also 8-byte aligned.
    #[repr(align(8))]
    struct AlignedBuf([UnsafeCell<MaybeUninit<u8>>; BUF_SIZE]);

    impl AlignedBuf {
        const fn new() -> Self {
            Self([const { UnsafeCell::new(MaybeUninit::new(0)) }; BUF_SIZE])
        }

        fn as_slice(&self) -> &[UnsafeCell<MaybeUninit<u8>>] {
            &self.0
        }
    }

    #[test]
    fn touching_no_boundaries() {
        let mut b = RingBuffer::new(1, 1);
        let buf = AlignedBuf::new();
        // SAFETY: Test buffer is well under i32::MAX / 4. `AlignedBuf` satisfies `ecc-64bit` alignment.
        let (mut p, mut c) = unsafe { b.split(buf.as_slice()) };
        p.write(&[1, 2]);

        let r = c.read();
        assert_eq!(r.bufs(), (&[1, 2][..], &[][..]));
        r.release(2);
        let r = c.read();
        assert_eq!(r.bufs(), (&[][..], &[][..]));
    }

    #[test]
    fn fill_simple() {
        let mut b = RingBuffer::new(0, 0);
        let buf = AlignedBuf::new();
        // SAFETY: Test buffer is well under i32::MAX / 4. `AlignedBuf` satisfies `ecc-64bit` alignment.
        let (mut p, mut c) = unsafe { b.split(buf.as_slice()) };
        p.write(&[1, 2, 3]);

        let r = c.read();
        assert_eq!(r.bufs(), (&[1, 2, 3][..], &[][..]));
        r.release(3);
        let r = c.read();
        assert_eq!(r.bufs(), (&[][..], &[][..]));
    }

    #[test]
    fn fill_crossing_end() {
        let buf = AlignedBuf::new();
        let start_pos = BUF_SIZE - 2;
        let mut b = RingBuffer::new(start_pos as u32, start_pos as u32);
        // SAFETY: Test buffer is well under i32::MAX / 4. `AlignedBuf` satisfies `ecc-64bit` alignment.
        let (mut p, mut c) = unsafe { b.split(buf.as_slice()) };
        p.write(&[1, 2, 3]);

        let r = c.read();
        assert_eq!(r.bufs(), (&[1, 2][..], &[3][..]));
        r.release(2);
        let r = c.read();
        assert_eq!(r.bufs(), (&[3][..], &[][..]));
        r.release(1);
        let r = c.read();
        assert_eq!(r.bufs(), (&[][..], &[][..]));
    }

    #[test]
    fn release_crossing_end() {
        let buf = AlignedBuf::new();
        let start_pos = BUF_SIZE - 2;
        let mut b = RingBuffer::new(start_pos as u32, start_pos as u32);
        // SAFETY: Test buffer is well under i32::MAX / 4. `AlignedBuf` satisfies `ecc-64bit` alignment.
        let (mut p, mut c) = unsafe { b.split(buf.as_slice()) };
        p.write(&[1, 2, 3]);

        let r = c.read();
        assert_eq!(r.bufs(), (&[1, 2][..], &[3][..]));
        r.release(3);
        let r = c.read();
        assert_eq!(r.bufs(), (&[][..], &[][..]));
    }

    #[test]
    fn underfill_crossing_end() {
        let buf = AlignedBuf::new();
        let start_pos = BUF_SIZE - 1;
        let mut b = RingBuffer::new(start_pos as u32, start_pos as u32);
        // SAFETY: Test buffer is well under i32::MAX / 4. `AlignedBuf` satisfies `ecc-64bit` alignment.
        let (mut p, mut c) = unsafe { b.split(buf.as_slice()) };
        p.write(&[1, 2]);

        let r = c.read();
        assert_eq!(r.bufs(), (&[1][..], &[2][..]));
        r.release(1);
        let r = c.read();
        assert_eq!(r.bufs(), (&[2][..], &[][..]));
        r.release(1);
        let r = c.read();
        assert_eq!(r.bufs(), (&[][..], &[][..]));
    }

    #[test]
    fn overfill() {
        let mut b = RingBuffer::new(0, 0);
        let buf = AlignedBuf::new();
        // SAFETY: Test buffer is well under i32::MAX / 4. `AlignedBuf` satisfies `ecc-64bit` alignment.
        let (mut p, mut c) = unsafe { b.split(buf.as_slice()) };
        // Write more than buffer can hold (BUF_SIZE - 1 is max capacity).
        p.write(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17]);

        let r = c.read();
        let (a, b) = r.bufs();
        assert_eq!(a.len() + b.len(), BUF_SIZE - 1);
        r.release(BUF_SIZE - 1);
        let r = c.read();
        assert_eq!(r.bufs(), (&[][..], &[][..]));
    }

    #[test]
    fn stop_at_end() {
        let buf = AlignedBuf::new();
        let start_pos = BUF_SIZE / 2;
        let mut b = RingBuffer::new(start_pos as u32, start_pos as u32);
        // SAFETY: Test buffer is well under i32::MAX / 4. `AlignedBuf` satisfies `ecc-64bit` alignment.
        let (mut p, mut c) = unsafe { b.split(buf.as_slice()) };
        p.write(&[1, 2]);

        let r = c.read();
        assert_eq!(r.bufs(), (&[1, 2][..], &[][..]));
        r.release(2);
        let r = c.read();
        assert_eq!(r.bufs(), (&[][..], &[][..]));
    }

    #[test]
    fn stop_before_end() {
        let buf = AlignedBuf::new();
        let start_pos = BUF_SIZE / 2;
        let mut b = RingBuffer::new(start_pos as u32, start_pos as u32);
        // SAFETY: Test buffer is well under i32::MAX / 4. `AlignedBuf` satisfies `ecc-64bit` alignment.
        let (mut p, mut c) = unsafe { b.split(buf.as_slice()) };
        p.write(&[1]);

        let r = c.read();
        assert_eq!(r.bufs(), (&[1][..], &[][..]));
        r.release(1);
        let r = c.read();
        assert_eq!(r.bufs(), (&[][..], &[][..]));
    }

    #[test]
    fn zero_release() {
        let buf = AlignedBuf::new();
        let start_pos = BUF_SIZE / 2;
        let mut b = RingBuffer::new(start_pos as u32, start_pos as u32);
        // SAFETY: Test buffer is well under i32::MAX / 4. `AlignedBuf` satisfies `ecc-64bit` alignment.
        let (mut p, mut c) = unsafe { b.split(buf.as_slice()) };
        p.write(&[1, 2]);

        let r = c.read();
        assert_eq!(r.bufs(), (&[1, 2][..], &[][..]));
        r.release(0);
        let r = c.read();
        assert_eq!(r.bufs(), (&[1, 2][..], &[][..]));
    }

    #[test]
    fn partial_release() {
        let buf = AlignedBuf::new();
        let start_pos = BUF_SIZE / 2;
        let mut b = RingBuffer::new(start_pos as u32, start_pos as u32);
        // SAFETY: Test buffer is well under i32::MAX / 4. `AlignedBuf` satisfies `ecc-64bit` alignment.
        let (mut p, mut c) = unsafe { b.split(buf.as_slice()) };
        p.write(&[1, 2]);

        let r = c.read();
        assert_eq!(r.bufs(), (&[1, 2][..], &[][..]));
        r.release(1);
        let r = c.read();
        assert_eq!(r.bufs(), (&[2][..], &[][..]));
    }
}
