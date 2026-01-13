//! A single-producer, single-consumer (SPSC) lock-free queue.

use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    ops::Range,
    ptr, slice,
    sync::atomic::{AtomicU32, Ordering, compiler_fence, fence},
};

/// A single-producer, single-consumer (SPSC) lock-free queue storing up to `len-1` bytes.
/// `len` is defined by the leftover size of the region after the [`RingBuffer`] has taken its
/// size.
///
/// # ECC Flush
///
/// On MCUs with 32-bit or 64-bit ECC-protected RAM (e.g., STM32H7/H5), writes are cached
/// until a full ECC word is written. A reset before the cache is flushed can lose data.
///
/// Enable the `ecc` feature to add an `_ecc_flush` field. After each write operation,
/// a single byte is written to this field, which flushes the ECC write cache by performing
/// an unaligned access to a different SRAM word.
///
/// Note: The struct layout changes with this feature, so the MAGIC value differs to
/// force reinitialization when switching between configurations.
///
/// # CPU Data Cache
///
/// On Cortex-M7 and other cores with a data cache, ensure the persist memory region is
/// configured as non-cacheable via the MPU. Otherwise, data may be lost in the CPU cache
/// on reset, even with ECC flushing enabled. Cortex-M0/M0+/M3/M4 do not have a data cache.
#[repr(C)]
pub struct RingBuffer {
    /// If the value is [`MAGIC`], the struct is initialized.
    ///
    /// In particular, this means that the reader-owned part of the buffer
    /// contains real data.
    header: u128,
    /// This contains the marker which identifies the buffer contents.
    identifier: Identifier,
    /// Where the next read starts.
    ///
    /// The RingBuffer always guarantees `read < len`.
    read: AtomicU32,
    /// Where the next write starts.
    ///
    /// The RingBuffer always guarantees `write < len`.
    write: AtomicU32,
    /// Writing a single byte to this field flushes the ECC write cache.
    /// An unaligned write to a different SRAM word forces the cache to commit.
    #[cfg(feature = "ecc")]
    _ecc_flush: UnsafeCell<u64>,
}

/// Identifier for the firmware that generated a log. It's up
/// to the implementer to use as many bytes as they deem necessary.
#[repr(transparent)]
#[derive(Clone, defmt::Format, Debug, PartialEq, Eq)]
pub struct Identifier(pub [u8; 16]);

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
    pub(crate) header: &'a RingBuffer,
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
/// The `ecc` layout uses a different magic to force reinitialization when switching.
#[cfg(not(feature = "ecc"))]
const MAGIC: u128 = 0xcd6f_a223_a2d9_29e5_2329_b0b4_5235_fb83;
#[cfg(feature = "ecc")]
const MAGIC: u128 = 0x3b50_0bf7_63b0_a8eb_d3f8_de4c_ab9e_9655;

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
            identifier: Identifier([0; 16]),
            read: AtomicU32::new(read),
            write: AtomicU32::new(write),
            #[cfg(feature = "ecc")]
            _ecc_flush: UnsafeCell::new(0),
        }
    }

    /// Flush the ECC write cache by writing a single byte to the flush field.
    ///
    /// No-op when `ecc` feature is disabled.
    #[inline]
    pub(crate) fn flush_ecc(&self) {
        #[cfg(feature = "ecc")]
        {
            // Ensure previous writes are emitted before the volatile write.
            compiler_fence(Ordering::SeqCst);
            // SAFETY: Writing a single byte to our own `UnsafeCell` field is safe.
            // This unaligned access to a different SRAM word flushes the ECC cache.
            // Concurrent writes from Producer and Consumer are safe because:
            // - Single-byte writes are atomic on all supported platforms.
            // - The value written is always 0; we don't care about the result.
            unsafe {
                let ptr: *mut u8 = self._ecc_flush.get().cast();
                ptr.write_volatile(0);
            }
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
    ) -> (Producer<'static>, Consumer<'static>, *mut Identifier) {
        let v: *mut Self = ptr::with_exposed_provenance_mut(memory.start);
        let buf_len = memory.len() - size_of::<RingBuffer>();

        // SAFETY:
        // - Alignment is guaranteed by the caller.
        // - Size is guaranteed by the caller.
        // - All fields (`u128`, `AtomicU32`, `UnsafeCell<u64>`, `[UnsafeCell<MaybeUninit<u8>>, X]`)
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
            // The intermediate state doesn't matter until header == MAGIC
            v.write.store(0, Ordering::Relaxed);
            v.flush_ecc();

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
            v.flush_ecc();
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

        let identifier = ptr::from_mut(&mut v.identifier);

        // SAFETY: The caller guarantees buf.len() < i32::MAX / 4.
        let (p, c) = unsafe { v.split(buf) };

        (p, c, identifier)
    }

    /// Splits the queue into producer and consumer given a memory area.
    ///
    /// # Safety
    ///
    /// `buf.len()` must be less than `i32::MAX / 4` to avoid overflow in pointer arithmetic.
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

        // Flush data before updating index. With 32-bit ECC, the index store may flush
        // immediately while data is still cached. This ensures the index never points
        // to uncommitted data.
        self.header.flush_ecc();

        self.header.write.store(
            (write.wrapping_add(len) % self.buf.len()) as u32,
            Ordering::Release,
        );
        self.header.flush_ecc();
    }
}

impl Consumer<'_> {
    /// Access the current identifier (as set by [`init`](crate::init)).
    ///
    /// This returns the identifier returned by the closure passed to `init()`,
    /// not the identifier of the firmware that produced the recovered logs.
    /// For that, use [`ConsumerAndMetadata::recovered_identifier`](crate::ConsumerAndMetadata::recovered_identifier).
    pub fn identifier(&self) -> &Identifier {
        &self.header.identifier
    }

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
/// Obtained from [`Consumer::read`]. The grant provides slices of available data
/// via [`GrantR::bufs`]. When done reading, call [`GrantR::release`] to mark bytes
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
// - release() only performs atomic stores to header.read (and `_ecc_flush` for ECC)
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
        self.consumer.header.flush_ecc();
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

    #[test]
    fn touching_no_boundaries() {
        let mut b = RingBuffer::new(1, 1);
        let buf = &[const { UnsafeCell::new(MaybeUninit::uninit()) }; 4];
        // SAFETY: Test buffer is 4 bytes, well under i32::MAX / 4.
        let (mut p, mut c) = unsafe { b.split(buf) };
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
        let buf = &[const { UnsafeCell::new(MaybeUninit::uninit()) }; 4];
        // SAFETY: Test buffer is 4 bytes, well under i32::MAX / 4.
        let (mut p, mut c) = unsafe { b.split(buf) };
        p.write(&[1, 2, 3]);

        let r = c.read();
        assert_eq!(r.bufs(), (&[1, 2, 3][..], &[][..]));
        r.release(3);
        let r = c.read();
        assert_eq!(r.bufs(), (&[][..], &[][..]));
    }

    #[test]
    fn fill_crossing_end() {
        let mut b = RingBuffer::new(2, 2);
        let buf = &[const { UnsafeCell::new(MaybeUninit::uninit()) }; 4];
        // SAFETY: Test buffer is 4 bytes, well under i32::MAX / 4.
        let (mut p, mut c) = unsafe { b.split(buf) };
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
        let mut b = RingBuffer::new(2, 2);
        let buf = &[const { UnsafeCell::new(MaybeUninit::uninit()) }; 4];
        // SAFETY: Test buffer is 4 bytes, well under i32::MAX / 4.
        let (mut p, mut c) = unsafe { b.split(buf) };
        p.write(&[1, 2, 3]);

        let r = c.read();
        assert_eq!(r.bufs(), (&[1, 2][..], &[3][..]));
        r.release(3);
        let r = c.read();
        assert_eq!(r.bufs(), (&[][..], &[][..]));
    }

    #[test]
    fn underfill_crossing_end() {
        let mut b = RingBuffer::new(3, 3);
        let buf = &[const { UnsafeCell::new(MaybeUninit::uninit()) }; 4];
        // SAFETY: Test buffer is 4 bytes, well under i32::MAX / 4.
        let (mut p, mut c) = unsafe { b.split(buf) };
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
        let buf = &[const { UnsafeCell::new(MaybeUninit::uninit()) }; 4];
        // SAFETY: Test buffer is 4 bytes, well under i32::MAX / 4.
        let (mut p, mut c) = unsafe { b.split(buf) };
        p.write(&[1, 2, 3, 4, 5, 6, 7]);

        let r = c.read();
        assert_eq!(r.bufs(), (&[1, 2, 3][..], &[][..]));
        r.release(3);
        let r = c.read();
        assert_eq!(r.bufs(), (&[][..], &[][..]));
    }

    #[test]
    fn stop_at_end() {
        let mut b = RingBuffer::new(2, 2);
        let buf = &[const { UnsafeCell::new(MaybeUninit::uninit()) }; 4];
        // SAFETY: Test buffer is 4 bytes, well under i32::MAX / 4.
        let (mut p, mut c) = unsafe { b.split(buf) };
        p.write(&[1, 2]);

        let r = c.read();
        assert_eq!(r.bufs(), (&[1, 2][..], &[][..]));
        r.release(2);
        let r = c.read();
        assert_eq!(r.bufs(), (&[][..], &[][..]));
    }

    #[test]
    fn stop_before_end() {
        let mut b = RingBuffer::new(2, 2);
        let buf = &[const { UnsafeCell::new(MaybeUninit::uninit()) }; 4];
        // SAFETY: Test buffer is 4 bytes, well under i32::MAX / 4.
        let (mut p, mut c) = unsafe { b.split(buf) };
        p.write(&[1]);

        let r = c.read();
        assert_eq!(r.bufs(), (&[1][..], &[][..]));
        r.release(1);
        let r = c.read();
        assert_eq!(r.bufs(), (&[][..], &[][..]));
    }

    #[test]
    fn zero_release() {
        let mut b = RingBuffer::new(2, 2);
        let buf = &[const { UnsafeCell::new(MaybeUninit::uninit()) }; 4];
        // SAFETY: Test buffer is 4 bytes, well under i32::MAX / 4.
        let (mut p, mut c) = unsafe { b.split(buf) };
        p.write(&[1, 2]);

        let r = c.read();
        assert_eq!(r.bufs(), (&[1, 2][..], &[][..]));
        r.release(0);
        let r = c.read();
        assert_eq!(r.bufs(), (&[1, 2][..], &[][..]));
    }

    #[test]
    fn partial_release() {
        let mut b = RingBuffer::new(2, 2);
        let buf = &[const { UnsafeCell::new(MaybeUninit::uninit()) }; 4];
        // SAFETY: Test buffer is 4 bytes, well under i32::MAX / 4.
        let (mut p, mut c) = unsafe { b.split(buf) };
        p.write(&[1, 2]);

        let r = c.read();
        assert_eq!(r.bufs(), (&[1, 2][..], &[][..]));
        r.release(1);
        let r = c.read();
        assert_eq!(r.bufs(), (&[2][..], &[][..]));
    }
}
