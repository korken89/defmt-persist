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
#[derive(Clone, defmt::Format, Debug, PartialEq, Eq, Default)]
pub struct Identifier(pub [u8; 16]);

impl Identifier {
    fn read_volatile(&mut self) -> Self {
        let identifier = ptr::from_mut(self);
        // SAFETY: A regular read from self would be safe here, but could be
        // optimized away.
        unsafe { identifier.read_volatile() }
    }

    fn write_volatile(&mut self, value: Self) {
        let identifier = ptr::from_mut(self);
        // SAFETY: A regular assignment to self would be safe here, but is not
        // guaranteed to actually update memory.
        unsafe { identifier.write_volatile(value) };
    }
}

/// Index math for a ring of `len` slots.
///
/// The used slots go from a start index up to an end index. One slot always stays free, so
/// that `start == end` means empty.
#[derive(Clone, Copy)]
struct Ring(usize);

impl Ring {
    /// Returns `true` if `index` is in the ring.
    fn contains(self, index: usize) -> bool {
        index < self.0
    }

    /// Number of used slots from `start` up to `end`.
    fn used(self, start: usize, end: usize) -> usize {
        if end >= start {
            end - start
        } else {
            self.0 - start + end
        }
    }

    /// Number of slots that can be used after `end` without reaching `start`.
    fn free(self, start: usize, end: usize) -> usize {
        self.0 - 1 - self.used(start, end)
    }

    /// The index `n` slots after `index`.
    fn add(self, index: usize, n: usize) -> usize {
        (index + n) % self.0
    }

    /// The `n` slots after `start`, split where they wrap around.
    ///
    /// For `start` in the ring and `n < len`, both ranges are in `0..len`.
    fn ranges(self, start: usize, n: usize) -> (Range<usize>, Range<usize>) {
        let before_end = n.min(self.0 - start);
        (start..start + before_end, 0..n - before_end)
    }
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
    fn flush_ecc(&self) {
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
        new_identifier: Identifier,
    ) -> (Producer<'static>, Consumer<'static>, Option<Identifier>) {
        let buf_len = memory.len() - size_of::<RingBuffer>();

        // SAFETY: This function's safety contract is a superset of the callee's.
        let v = unsafe { Self::from_memory(&memory) };
        let old_identifier = if !v.header_valid() {
            v.read.store(0, Ordering::Relaxed);
            // The intermediate state doesn't matter until header == MAGIC
            v.write.store(0, Ordering::Relaxed);
            v.identifier.write_volatile(new_identifier);

            // The identifier must be in memory before the header claims it
            // is valid, otherwise a reset in between recovers garbage.
            v.set_header(MAGIC);
            None
        } else {
            v.repair(buf_len);

            let old_identifier = v.identifier.read_volatile();
            if old_identifier != new_identifier {
                // Temporarily invalidate the header during the
                // non-atomic write to identifier, otherwise a reset in
                // between recovers garbage.
                v.set_header(0);
                v.identifier.write_volatile(new_identifier);
                v.set_header(MAGIC);
            }
            Some(old_identifier)
        };
        v.flush_ecc();
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

        // SAFETY: The caller guarantees buf.len() < i32::MAX / 4.
        let (p, c) = unsafe { v.split(buf) };

        (p, c, old_identifier)
    }

    /// Reinterprets `memory` as a `RingBuffer` without checking the header.
    ///
    /// # Safety
    ///
    /// - `memory.start` must be aligned to `align_of::<RingBuffer>()`.
    /// - `memory.len()` must be greater than `size_of::<RingBuffer>()`.
    /// - Buffer size (`memory.len() - size_of::<RingBuffer>()`) must be less than
    ///   `i32::MAX / 4` to avoid overflow in pointer arithmetic.
    /// - No other reference to the `RingBuffer` in `memory` may be live during `'a`.
    unsafe fn from_memory<'a>(memory: &Range<usize>) -> &'a mut Self {
        let v: *mut Self = ptr::with_exposed_provenance_mut(memory.start);
        // SAFETY:
        // - Alignment is guaranteed by the caller.
        // - Size is guaranteed by the caller.
        // - All fields (`u128`, `AtomicU32`, `UnsafeCell<u64>`, `[UnsafeCell<MaybeUninit<u8>>, X]`)
        //   are valid for any bit pattern, so interpreting the raw memory as this
        //   type and buffer is sound. As the memory is initialized outside the Rust abstract
        //   machine (of the running program), we consider the caveats of non-fixed
        //   bit patterns from `MaybeUninit` mitigated.
        // - The caller guarantees no aliasing references exist during `'a`.
        unsafe { &mut *v }
    }

    /// Returns `true` if the header marks the contents as initialized.
    fn header_valid(&mut self) -> bool {
        let header = ptr::from_mut(&mut self.header);
        // SAFETY: A regular read from self.header would be safe here, but it
        // could be optimized away.
        unsafe { header.read_volatile() == MAGIC }
    }

    /// Writes `value` to the header, ordered after all previous writes.
    fn set_header(&mut self, value: u128) {
        self.flush();
        let header = ptr::from_mut(&mut self.header);
        // SAFETY: A regular assignment to self.header would be safe here, but
        // is not guaranteed to actually update memory.
        unsafe { header.write_volatile(value) };
        self.flush();
    }

    /// Brings the indexes of a recovered buffer into a valid state.
    fn repair(&mut self, buf_len: usize) {
        let (read, write) = self.repaired_indexes(buf_len);
        // Since `header` can be marked as valid, some extra care
        // is taken here to avoid situations where there is a gap of time
        // where both indexes are in-bounds, but not valid. Otherwise
        // a poorly timed reset could leave the queue in a state that
        // appears valid and non-empty. Storing `read` first ensures this,
        // as a still invalid `write` is then repaired to `read` again.
        if self.read.load(Ordering::Relaxed) as usize != read {
            self.read.store(read as u32, Ordering::Relaxed);
        }
        if self.write.load(Ordering::Relaxed) as usize != write {
            self.write.store(write as u32, Ordering::Relaxed);
        }
    }

    /// The `read` and `write` indexes that [`Self::repair`] brings the buffer to.
    fn repaired_indexes(&self, buf_len: usize) -> (usize, usize) {
        // Even if the header promises to keep the contract, we don't
        // trust it for the safety of our pointer offsets.
        let read = self.read.load(Ordering::Relaxed) as usize;
        let write = self.write.load(Ordering::Relaxed) as usize;
        let ring = Ring(buf_len);
        match (ring.contains(read), ring.contains(write)) {
            (true, true) => (read, write),
            (true, false) => (read, read),
            (false, true) => (write, write),
            (false, false) => (0, 0),
        }
    }

    /// Orders all previous writes before the following ones, also across a reset.
    fn flush(&self) {
        self.flush_ecc();
        fence(Ordering::SeqCst);
    }

    /// # Safety
    ///
    /// - `memory.start` must be aligned to `align_of::<RingBuffer>()`.
    /// - `memory.len()` must be greater than `size_of::<RingBuffer>()`.
    /// - Buffer size (`memory.len() - size_of::<RingBuffer>()`) must be less than
    ///   `i32::MAX / 4` to avoid overflow in pointer arithmetic.
    /// - No other access to `memory` for the duration of the function call.
    pub(crate) unsafe fn peek_identifier(memory: Range<usize>) -> Option<Identifier> {
        // SAFETY: The safety contract is reexported.
        let v = unsafe { Self::from_memory(&memory) };
        v.header_valid().then(|| v.identifier.read_volatile())
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
    /// Appends `data` to the buffer.
    ///
    /// If there is not enough space, the last bytes are silently discarded.
    #[inline]
    pub fn write(&mut self, data: &[u8]) {
        // Relaxed: stale `read` is safe (underestimates available space).
        let read = self.header.read.load(Ordering::Relaxed) as usize;
        // Relaxed: producer owns `write`, no cross-thread synchronization needed.
        let write = self.header.write.load(Ordering::Relaxed) as usize;
        let ring = Ring(self.buf.len());
        let len = data.len().min(ring.free(read, write));
        if len == 0 {
            return;
        }

        let buf: *mut u8 = self.buf.as_ptr().cast_mut().cast();
        let (range1, range2) = ring.ranges(write, len);
        let (data1, data2) = data[..len].split_at(range1.len());
        for (range, data) in [(range1, data1), (range2, data2)] {
            // SAFETY:
            // - src valid: `data` is a sub-slice of the caller's data.
            // - dst valid: `range` is in `buf`, since `write` is in the ring and
            //   `len < buf.len()`. It is owned by the producer, since `len` is at
            //   most the free space after `write`.
            // - aligned: u8 slices have alignment 1.
            // - nonoverlapping: The caller-provided `data` cannot overlap with the part of `buf`
            //   owned by the producer, because only the consumer gives slices to external code.
            // - `pointer::add` stays in `buf`, and the only constructor `RingBuffer::split`
            //   requires of its caller that the offset fits in `isize`.
            unsafe { ptr::copy_nonoverlapping(data.as_ptr(), buf.add(range.start), data.len()) };
        }

        // Flush data before updating index. With 32-bit ECC, the index store may flush
        // immediately while data is still cached. This ensures the index never points
        // to uncommitted data.
        self.header.flush_ecc();

        self.header
            .write
            .store(ring.add(write, len) as u32, Ordering::Release);
        self.header.flush_ecc();
    }
}

impl Consumer<'_> {
    /// Access the current identifier (as set by [`init`](crate::init)).
    ///
    /// This returns the identifier set by `init()`, not the identifier
    /// of the firmware that produced the recovered logs. For that, use
    /// [`ConsumerAndMetadata::recovered_identifier`](crate::ConsumerAndMetadata::recovered_identifier).
    pub fn identifier(&self) -> &Identifier {
        &self.header.identifier
    }

    /// Number of bytes available to read, including data from an older firmware.
    pub(crate) fn len(&self) -> usize {
        let write = self.header.write.load(Ordering::Acquire) as usize;
        let read = self.header.read.load(Ordering::Relaxed) as usize;
        Ring(self.buf.len()).used(read, write)
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
    /// function will provide two slices.
    #[inline]
    #[must_use]
    pub fn read(&mut self) -> GrantR<'_, '_> {
        // Acquire: synchronizes with producer's Release store, ensuring we see the written data.
        let write = self.header.write.load(Ordering::Acquire) as usize;
        // Relaxed: consumer owns `read`, no cross-thread synchronization needed.
        let read = self.header.read.load(Ordering::Relaxed) as usize;
        let ring = Ring(self.buf.len());
        let (range1, range2) = ring.ranges(read, ring.used(read, write));
        // SAFETY: The ranges are the data from `read` up to `write`.
        let slice1 = unsafe { self.slice(range1) };
        // SAFETY: As above.
        let slice2 = unsafe { self.slice(range2) };
        GrantR {
            consumer: self,
            slice1,
            slice2,
            original_read: read,
        }
    }

    /// The bytes of `buf` in `range`.
    ///
    /// # Safety
    ///
    /// `range` must be in `buf` and owned by the consumer.
    unsafe fn slice(&self, range: Range<usize>) -> &[u8] {
        let buf: *const u8 = self.buf.as_ptr().cast();
        // SAFETY:
        // For `slice::from_raw_parts`:
        // - Non-null, valid, aligned: it is a sub-slice of `buf`, guaranteed by the caller.
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
        // For `pointer::add`: The offset stays in `buf`, and the only constructor
        // `RingBuffer::split` requires of its caller that it fits in `isize`.
        unsafe { slice::from_raw_parts(buf.add(range.start), range.len()) }
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
        let read = Ring(self.consumer.buf.len()).add(self.original_read, used);
        self.consumer
            .header
            .read
            .store(read as u32, Ordering::Release);
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

    extern crate std;
    use core::mem::offset_of;
    use std::{vec, vec::Vec};

    /// Persist memory with a 16 byte buffer, reused across simulated resets.
    #[repr(C, align(16))]
    struct Memory([u8; size_of::<RingBuffer>() + 16]);

    impl Memory {
        fn new() -> Self {
            Memory([0; size_of::<RingBuffer>() + 16])
        }

        fn range(&mut self) -> Range<usize> {
            let start = ptr::from_mut(self).expose_provenance();
            start..start + size_of::<Self>()
        }

        /// Initializes the buffer as the firmware identified by `id` would.
        fn boot(&mut self, id: u8) -> (Producer<'_>, Consumer<'_>, Option<Identifier>) {
            let range = self.range();
            // SAFETY: The memory is aligned and large enough, and borrowed for
            // as long as the producer and consumer live.
            unsafe { RingBuffer::recover_or_reinitialize(range, Identifier([id; 16])) }
        }

        fn peek(&mut self) -> Option<Identifier> {
            let range = self.range();
            // SAFETY: The memory is aligned, large enough and borrowed.
            unsafe { RingBuffer::peek_identifier(range) }
        }

        fn set_u32(&mut self, offset: usize, value: u32) {
            self.0[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
        }
    }

    /// Reads everything, returning the data of each grant.
    fn drain(c: &mut Consumer<'_>) -> Vec<Vec<u8>> {
        let mut grants = Vec::new();
        while !c.is_empty() {
            let r = c.read();
            let (buf1, buf2) = r.bufs();
            let data = [buf1, buf2].concat();
            // Guards against looping forever on empty grants.
            assert!(!data.is_empty());
            grants.push(data);
            r.release_all();
        }
        grants
    }

    #[test]
    fn ring_math() {
        for len in 1..6 {
            let ring = Ring(len);
            for start in 0..len {
                for end in 0..len {
                    let used = ring.used(start, end);
                    assert_eq!(used + ring.free(start, end), len - 1);
                    assert_eq!(ring.add(start, used), end);
                    let (range1, range2) = ring.ranges(start, used);
                    let indexes: Vec<_> = range1.chain(range2).collect();
                    let expected: Vec<_> = (0..used).map(|n| (start + n) % len).collect();
                    assert_eq!(indexes, expected);
                }
            }
        }
    }

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

    #[test]
    fn recovers_data_after_reset() {
        let mut m = Memory::new();
        assert_eq!(m.peek(), None);
        let (mut p, _, recovered) = m.boot(1);
        assert_eq!(recovered, None);
        p.write(&[1, 2]);

        let (_, mut c, recovered) = m.boot(2);
        assert_eq!(recovered, Some(Identifier([1; 16])));
        assert_eq!(c.len(), 2);
        assert_eq!(drain(&mut c), [vec![1, 2]]);
        assert_eq!(m.peek(), Some(Identifier([2; 16])));
    }

    #[test]
    fn recovers_data_crossing_end() {
        let mut m = Memory::new();
        let (mut p, mut c, _) = m.boot(1);
        p.write(&[9; 13]);
        drain(&mut c);
        p.write(&[1; 4]);

        let (_, mut c, _) = m.boot(1);
        assert_eq!(c.len(), 4);
        assert_eq!(drain(&mut c), [vec![1; 4]]);
    }

    #[test]
    fn invalid_indexes_are_repaired() {
        let read = offset_of!(RingBuffer, read);
        let write = offset_of!(RingBuffer, write);
        for (read_value, write_value) in [(100, 2), (0, 100), (100, 100)] {
            let mut m = Memory::new();
            let (mut p, _, _) = m.boot(1);
            p.write(&[1, 1]);

            m.set_u32(read, read_value);
            m.set_u32(write, write_value);
            let (mut p, mut c, recovered) = m.boot(1);
            assert_eq!(recovered, Some(Identifier([1; 16])));
            assert_eq!(drain(&mut c), Vec::<Vec<u8>>::new());
            p.write(&[2]);
            assert_eq!(drain(&mut c), [vec![2]]);
        }
    }

    #[test]
    fn invalid_header_reinitializes() {
        let mut m = Memory::new();
        let (mut p, _, _) = m.boot(1);
        p.write(&[1, 1]);

        m.set_u32(offset_of!(RingBuffer, header), 0);
        let (_, mut c, recovered) = m.boot(1);
        assert_eq!(recovered, None);
        assert_eq!(drain(&mut c), Vec::<Vec<u8>>::new());
    }
}
