//! A single-producer, single-consumer (SPSC) lock-free queue.

use core::{cell::UnsafeCell, mem::MaybeUninit, ops::Range, ptr, slice, sync::atomic::Ordering};

use crate::sync::{AtomicU8, AtomicU32, fence};

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
    /// The firmwares that wrote the buffer contents.
    segments: Segments,
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
    _ecc_flush: EccFlush,
    /// One cell per byte of the buffer, for loom to track the accesses to it.
    #[cfg(loom)]
    loom_bytes: [loom::cell::UnsafeCell<()>; LOOM_BUF_LEN],
}

/// The firmwares that wrote the buffer contents, oldest first.
///
/// The data of each segment ends where the next one starts, and the newest one ends at
/// `write`. The RingBuffer always guarantees that there are 1 to [`SEGMENTS`] segments, and
/// that the start of every segment except the oldest lies between `read` and `write`, in order.
/// The data of the oldest segment starts at `read`, so its own start is unused.
#[repr(C)]
struct Segments {
    /// A ring of segments from `first` up to `last`.
    table: [Segment; SEGMENTS + 1],
    /// Index of the oldest segment.
    ///
    /// The RingBuffer always guarantees `first <= SEGMENTS`.
    first: AtomicU8,
    /// Index after the newest segment, which is a free slot.
    ///
    /// The RingBuffer always guarantees `last <= SEGMENTS` and `last != first`.
    last: AtomicU8,
}

/// Data written by one firmware.
#[repr(C)]
struct Segment {
    identifier: Identifier,
    /// Where the data starts.
    start: AtomicU32,
}

/// The length of the buffer in the loom model.
#[cfg(loom)]
const LOOM_BUF_LEN: usize = 4;

/// A byte in an SRAM word of its own.
#[cfg(feature = "ecc")]
#[repr(C, align(8))]
struct EccFlush(AtomicU8);

/// Identifier for the firmware that generated a log. It's up
/// to the implementer to use as many bytes as they deem necessary.
#[repr(transparent)]
#[derive(Clone, defmt::Format, Debug, PartialEq, Eq, Default)]
pub struct Identifier(pub [u8; 16]);

impl Identifier {
    fn read_volatile(&self) -> Self {
        let identifier = ptr::from_ref(self);
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

    /// The index `n` slots before `index`.
    fn sub(self, index: usize, n: usize) -> usize {
        (index + self.0 - n) % self.0
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
// - After initialization only the Consumer writes `segments.first`, and the rest of
//   `segments` is not modified
// - The UnsafeCell slice is only accessed through methods that maintain the SPSC invariant
unsafe impl Send for Consumer<'_> {}

/// Value used to indicate that the queue is initialized.
///
/// Replace this if the layout or field semantics change in a backwards-incompatible way.
/// The `ecc` layout uses a different magic to force reinitialization when switching.
#[cfg(not(feature = "ecc"))]
const MAGIC: u128 = 0x175f_6734_765d_3b94_cf5c_4898_7eb7_b183;
#[cfg(feature = "ecc")]
const MAGIC: u128 = 0x7ba8_6088_fd5d_4e6e_e192_ffed_fbd9_cea8;

/// Number of segments, where a new one starts each time a different firmware logs.
const SEGMENTS: usize = 8;
const _: () = assert!(SEGMENTS <= u8::MAX as usize, "segment indexes are u8");

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
            segments: Segments {
                table: core::array::from_fn(|_| Segment {
                    identifier: Identifier([0; 16]),
                    start: AtomicU32::new(0),
                }),
                first: AtomicU8::new(0),
                last: AtomicU8::new(1),
            },
            read: AtomicU32::new(read),
            write: AtomicU32::new(write),
            #[cfg(feature = "ecc")]
            _ecc_flush: EccFlush(AtomicU8::new(0)),
            #[cfg(loom)]
            loom_bytes: core::array::from_fn(|_| loom::cell::UnsafeCell::new(())),
        }
    }

    /// Flush the ECC write cache by writing a single byte to the flush field.
    ///
    /// No-op when `ecc` feature is disabled.
    #[inline]
    fn flush_ecc(&self) {
        // Where a reset may happen, for the proofs to check.
        #[cfg(kani)]
        proofs::reset_point(self);
        #[cfg(feature = "ecc")]
        {
            // Ensure previous writes are emitted before the flush.
            crate::sync::compiler_fence(Ordering::SeqCst);
            // Atomic, since the Producer and the Consumer both flush.
            self._ecc_flush.0.store(0, Ordering::Relaxed);
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
    ) -> (Producer<'static>, Consumer<'static>) {
        let buf_len = memory.len() - size_of::<RingBuffer>();

        // SAFETY: This function's safety contract is a superset of the callee's.
        let v = unsafe { Self::from_memory(&memory) };
        v.recover(buf_len, new_identifier);

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
        unsafe { v.split(buf) }
    }

    /// Keeps what can be trusted of a buffer of `buf_len` bytes from a previous run, and
    /// continues it with data from the firmware `new_identifier`.
    fn recover(&mut self, buf_len: usize, new_identifier: Identifier) {
        if self.header_valid() && self.repair(buf_len) {
            self.drop_empty_segments();
            if self.segments.newest().identifier.read_volatile() != new_identifier {
                self.push_segment(new_identifier);
            }
        } else {
            self.set_header(0);
            self.read.store(0, Ordering::Relaxed);
            self.write.store(0, Ordering::Relaxed);
            self.segments.first.store(0, Ordering::Relaxed);
            self.segments.last.store(1, Ordering::Relaxed);
            self.segments.table[0]
                .identifier
                .write_volatile(new_identifier);

            // Everything must be in memory before the header claims it is
            // valid, otherwise a reset in between recovers garbage.
            self.set_header(MAGIC);
        }
        self.flush();
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

    /// Brings the indexes and segments of a recovered buffer into a valid
    /// state, returning `false` if the segments can't be trusted.
    fn repair(&mut self, buf_len: usize) -> bool {
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
        self.valid(buf_len)
    }

    /// Returns `true` if the indexes and segments uphold what the RingBuffer guarantees,
    /// for a buffer of `buf_len` bytes.
    fn valid(&self, buf_len: usize) -> bool {
        let read = self.read.load(Ordering::Relaxed) as usize;
        let write = self.write.load(Ordering::Relaxed) as usize;
        let ring = Ring(buf_len);
        ring.contains(read) && ring.contains(write) && self.segments_valid(read, write, buf_len)
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

    /// Returns `true` if the segments describe the data from `read` up to `write`.
    fn segments_valid(&self, read: usize, write: usize, buf_len: usize) -> bool {
        if !self.segments.valid() {
            return false;
        }
        // The consumer trusts the segment starts as the end of a read, so
        // they must lie within the readable data, in order.
        let ring = Ring(buf_len);
        let readable = ring.used(read, write);
        let mut previous = 0;
        self.segments.later_starts().all(|start| {
            let used = ring.used(read, start);
            let ok = ring.contains(start) && previous <= used && used <= readable;
            previous = used;
            ok
        })
    }

    /// Drops the newest segments that hold no data, from firmwares that did not log.
    fn drop_empty_segments(&mut self) {
        let write = self.write.load(Ordering::Relaxed) as usize;
        while self.segments.count() > 1 && self.segments.newest().start() == write {
            let last = Segments::RING.sub(self.segments.last(), 1);
            self.segments.last.store(last as u8, Ordering::Relaxed);
            // `last` must reach memory before the freed slot is reused.
            self.flush();
        }
    }

    /// Starts a new segment at `write`, for data from the firmware `identifier`.
    ///
    /// A reset at any point leaves a valid state, since each change becomes visible through a
    /// single index store.
    fn push_segment(&mut self, identifier: Identifier) {
        let write = self.write.load(Ordering::Relaxed);

        if self.segments.count() == SEGMENTS {
            // The table is full, drop the data of the oldest firmware.
            self.drop_oldest_segment();
        }

        // The slot at `last` is always free.
        let last = self.segments.last();
        let segment = &mut self.segments.table[last];
        segment.identifier.write_volatile(identifier);
        segment.start.store(write, Ordering::Relaxed);
        // The slot and `first` must reach memory before `last`.
        self.flush();
        self.segments
            .last
            .store(Segments::RING.add(last, 1) as u8, Ordering::Relaxed);
        self.flush();
    }

    /// Drops the oldest segments that have been read completely.
    fn drop_read_segments(&self) {
        let read = self.read.load(Ordering::Relaxed) as usize;
        while self.segments.later_starts().next() == Some(read) {
            self.drop_oldest_segment();
        }
    }

    /// Drops the oldest segment, along with any of its data that is still unread.
    fn drop_oldest_segment(&self) {
        let second = Segments::RING.add(self.segments.first(), 1);
        let start = self.segments.table[second].start.load(Ordering::Relaxed);
        self.read.store(start, Ordering::Release);
        // `read` must reach memory before `first`, also across a reset.
        self.flush();
        self.segments.first.store(second as u8, Ordering::Relaxed);
        self.flush_ecc();
    }

    /// Orders all previous writes before the following ones, also across a reset.
    fn flush(&self) {
        self.flush_ecc();
        fence(Ordering::SeqCst);
    }

    /// The loom cells of `len` bytes of the buffer from `start`.
    #[cfg(loom)]
    fn loom_cells(
        &self,
        start: usize,
        len: usize,
        buf_len: usize,
    ) -> impl Iterator<Item = &loom::cell::UnsafeCell<()>> {
        assert_eq!(buf_len, LOOM_BUF_LEN);
        (start..start + len).map(move |index| &self.loom_bytes[index % buf_len])
    }

    /// # Safety
    ///
    /// - `memory.start` must be aligned to `align_of::<RingBuffer>()`.
    /// - `memory.len()` must be greater than `size_of::<RingBuffer>()`.
    /// - Buffer size (`memory.len() - size_of::<RingBuffer>()`) must be less than
    ///   `i32::MAX / 4` to avoid overflow in pointer arithmetic.
    /// - No other access to `memory` for the duration of the function call.
    pub(crate) unsafe fn peek_identifier(memory: Range<usize>) -> Option<Identifier> {
        let buf_len = memory.len() - size_of::<RingBuffer>();
        // SAFETY: The safety contract is reexported.
        let v = unsafe { Self::from_memory(&memory) };
        let (read, write) = v.repaired_indexes(buf_len);
        (v.header_valid() && v.segments_valid(read, write, buf_len))
            .then(|| v.segments.newest().identifier.read_volatile())
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

impl Segments {
    /// The ring of indexes into `table`.
    const RING: Ring = Ring(SEGMENTS + 1);

    fn first(&self) -> usize {
        self.first.load(Ordering::Relaxed) as usize
    }

    fn last(&self) -> usize {
        self.last.load(Ordering::Relaxed) as usize
    }

    /// Returns `true` if `first` and `last` describe 1 to [`SEGMENTS`] segments.
    fn valid(&self) -> bool {
        let (first, last) = (self.first(), self.last());
        Self::RING.contains(first) && Self::RING.contains(last) && first != last
    }

    fn count(&self) -> usize {
        Self::RING.used(self.first(), self.last())
    }

    fn newest(&self) -> &Segment {
        &self.table[Self::RING.sub(self.last(), 1)]
    }

    /// The starts of all segments except the oldest, oldest first.
    fn later_starts(&self) -> impl Iterator<Item = usize> {
        let first = self.first();
        (1..self.count()).map(move |n| self.table[Self::RING.add(first, n)].start())
    }
}

impl Segment {
    fn start(&self) -> usize {
        self.start.load(Ordering::Relaxed) as usize
    }
}

impl Producer<'_> {
    /// Appends `data` to the buffer.
    ///
    /// If there is not enough space, the last bytes are silently discarded.
    #[inline]
    pub fn write(&mut self, data: &[u8]) {
        // Acquire: synchronizes with consumer's Release store, so its reads of the freed
        // space happen before we overwrite it. A stale `read` underestimates the space.
        let read = self.header.read.load(Ordering::Acquire) as usize;
        // Relaxed: producer owns `write`, no cross-thread synchronization needed.
        let write = self.header.write.load(Ordering::Relaxed) as usize;
        let ring = Ring(self.buf.len());
        let len = data.len().min(ring.free(read, write));
        if len == 0 {
            return;
        }

        #[cfg(loom)]
        self.header
            .loom_cells(write, len, self.buf.len())
            .for_each(|cell| cell.with_mut(|_| ()));

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

impl<'c> Consumer<'c> {
    /// Access the current identifier (as set by [`init`](crate::init)).
    ///
    /// Recovered logs may have been written by an older firmware. Use
    /// [`GrantR::identifier`] to get the identifier of the data being read.
    pub fn identifier(&self) -> &'c Identifier {
        &self.header.segments.newest().identifier
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
    ///
    /// A grant only holds data written by one firmware, see [`GrantR::identifier`].
    #[inline]
    #[must_use]
    pub fn read(&mut self) -> GrantR<'_, 'c> {
        let header = self.header;
        // Acquire: synchronizes with producer's Release store, ensuring we see the written data.
        let write = header.write.load(Ordering::Acquire) as usize;
        // Relaxed: consumer owns `read`, no cross-thread synchronization needed.
        let read = header.read.load(Ordering::Relaxed) as usize;
        // The consumer owns `first` after initialization, and `last` is fixed.
        header.drop_read_segments();
        let segments = &header.segments;
        let end = segments.later_starts().next().unwrap_or(write);
        let identifier = &segments.table[segments.first()].identifier;
        let ring = Ring(self.buf.len());
        let (range1, range2) = ring.ranges(read, ring.used(read, end));
        // SAFETY: The ranges are the data from `read` up to `end`, which is at
        // most `write` by the invariants on `segments`.
        let slice1 = unsafe { self.slice(range1) };
        // SAFETY: As above.
        let slice2 = unsafe { self.slice(range2) };
        // The bytes of the grant may be read from here on.
        #[cfg(loom)]
        header
            .loom_cells(read, slice1.len() + slice2.len(), self.buf.len())
            .for_each(|cell| cell.with(|_| ()));
        GrantR {
            consumer: self,
            slice1,
            slice2,
            original_read: read,
            identifier,
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

    #[cfg(feature = "async-await")]
    /// Waits for data and passes up to `max_len` bytes of it to `f`, together
    /// with the identifier of the firmware that wrote it.
    ///
    /// The bytes are released when `f` resolves. If the future is dropped before
    /// that, nothing is released and the same bytes are passed on the next call.
    /// They may start or end in the middle of a defmt frame.
    pub async fn next_log<T>(
        &mut self,
        max_len: core::num::NonZeroUsize,
        f: impl AsyncFnOnce(&'c Identifier, &[u8]) -> T,
    ) -> T {
        self.wait_for_data().await;
        let grant = self.read();
        let (buf, _) = grant.bufs();
        let len = buf.len().min(max_len.get());
        let ret = f(grant.identifier(), &buf[..len]).await;
        grant.release(len);
        ret
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
    identifier: &'c Identifier,
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
        // The bytes of the grant may have been read up to here.
        #[cfg(loom)]
        self.consumer
            .header
            .loom_cells(
                self.original_read,
                self.slice1.len() + self.slice2.len(),
                self.consumer.buf.len(),
            )
            .for_each(|cell| cell.with(|_| ()));
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

    /// Identifies the firmware that wrote the bytes in this grant.
    ///
    /// The reference stays valid after the grant is released.
    #[inline]
    pub fn identifier(&self) -> &'c Identifier {
        self.identifier
    }
}

#[cfg(all(test, not(loom)))]
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
        fn boot(&mut self, id: u8) -> (Producer<'_>, Consumer<'_>) {
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

        /// Offset of segment slot `index`.
        fn slot_offset(index: usize) -> usize {
            offset_of!(RingBuffer, segments)
                + offset_of!(Segments, table)
                + index * size_of::<Segment>()
        }

        /// Offset of the start of segment slot `index`.
        fn start_offset(index: usize) -> usize {
            Self::slot_offset(index) + offset_of!(Segment, start)
        }

        fn set_u32(&mut self, offset: usize, value: u32) {
            self.0[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
        }

        /// Sets up a valid header with the indexes and the segments `(id, start)`,
        /// oldest first. Each data byte holds its position.
        fn set_state(&mut self, read: u32, write: u32, segments: &[(u8, u32)]) {
            self.boot(0);
            // Starts at the end of the table, so that the segments wrap around.
            let first = SEGMENTS;
            let last = Segments::RING.add(first, segments.len());
            let table = offset_of!(RingBuffer, segments);
            self.set_u32(offset_of!(RingBuffer, read), read);
            self.set_u32(offset_of!(RingBuffer, write), write);
            self.0[table + offset_of!(Segments, first)] = first as u8;
            self.0[table + offset_of!(Segments, last)] = last as u8;
            for (n, &(id, start)) in segments.iter().enumerate() {
                let index = Segments::RING.add(first, n);
                let identifier = Self::slot_offset(index) + offset_of!(Segment, identifier);
                self.0[identifier..identifier + 16].fill(id);
                self.set_u32(Self::start_offset(index), start);
            }
            let data = size_of::<RingBuffer>();
            for (position, byte) in self.0[data..].iter_mut().enumerate() {
                *byte = position as u8;
            }
        }
    }

    /// Number of bytes available to read.
    fn len(c: &Consumer<'_>) -> usize {
        let write = c.header.write.load(Ordering::Relaxed) as usize;
        let read = c.header.read.load(Ordering::Relaxed) as usize;
        Ring(c.buf.len()).used(read, write)
    }

    /// Reads everything, returning the identifier byte and the data of each grant.
    fn drain(c: &mut Consumer<'_>) -> Vec<(u8, Vec<u8>)> {
        let mut grants = Vec::new();
        while !c.is_empty() {
            let r = c.read();
            let (buf1, buf2) = r.bufs();
            let data = [buf1, buf2].concat();
            // Guards against looping forever on empty grants.
            assert!(!data.is_empty());
            grants.push((r.identifier().0[0], data));
            r.release_all();
        }
        grants
    }

    #[test]
    fn old_firmware_identified_after_another_reset() {
        let mut m = Memory::new();
        assert_eq!(m.peek(), None);
        let (mut p, _) = m.boot(1);
        p.write(&[1, 1]);

        assert_eq!(m.peek(), Some(Identifier([1; 16])));
        let (mut p, c) = m.boot(2);
        assert_eq!(len(&c), 2);
        p.write(&[2]);

        assert_eq!(m.peek(), Some(Identifier([2; 16])));
        let (_, mut c) = m.boot(2);
        // The identifier outlives the grant.
        let oldest = c.read().identifier();
        assert_eq!(drain(&mut c), [(1, vec![1, 1]), (2, vec![2])]);
        assert_eq!(oldest, &Identifier([1; 16]));
    }

    #[test]
    fn bootloader_and_application_both_log() {
        let mut m = Memory::new();
        for _ in 0..2 {
            let (mut p, _) = m.boot(9);
            p.write(&[9]);
            let (mut p, _) = m.boot(1);
            p.write(&[1]);
        }

        assert_eq!(m.peek(), Some(Identifier([1; 16])));
        let (_, mut c) = m.boot(9);
        let expected = [(9, vec![9]), (1, vec![1]), (9, vec![9]), (1, vec![1])];
        assert_eq!(drain(&mut c), expected);
    }

    #[test]
    fn full_table_drops_oldest_data() {
        let mut m = Memory::new();
        // Also wraps around the end of the table.
        for id in 1..=SEGMENTS as u8 + 2 {
            let (mut p, _) = m.boot(id);
            p.write(&[id]);
        }

        let (_, mut c) = m.boot(SEGMENTS as u8 + 2);
        let expected: Vec<_> = (3..=SEGMENTS as u8 + 2).map(|id| (id, vec![id])).collect();
        assert_eq!(drain(&mut c), expected);
    }

    #[test]
    fn firmwares_without_logs_keep_old_data() {
        let mut m = Memory::new();
        let (mut p, _) = m.boot(1);
        p.write(&[1]);
        for id in (2..=3).cycle().take(4 * SEGMENTS) {
            m.boot(id);
        }

        assert_eq!(m.peek(), Some(Identifier([3; 16])));
        let (mut p, mut c) = m.boot(4);
        assert_eq!(drain(&mut c), [(1, vec![1])]);
        p.write(&[4]);
        assert_eq!(drain(&mut c), [(4, vec![4])]);
    }

    #[test]
    fn partially_read_old_data_keeps_identifier() {
        let mut m = Memory::new();
        let (mut p, _) = m.boot(1);
        p.write(&[1, 1, 1]);

        let (mut p, mut c) = m.boot(2);
        c.read().release(1);
        p.write(&[2]);

        let (_, mut c) = m.boot(2);
        assert_eq!(drain(&mut c), [(1, vec![1, 1]), (2, vec![2])]);
    }

    #[test]
    fn old_data_crossing_end() {
        let mut m = Memory::new();
        let (mut p, mut c) = m.boot(1);
        p.write(&[9; 13]);
        drain(&mut c);
        p.write(&[1; 4]);

        let (mut p, _) = m.boot(2);
        p.write(&[2; 2]);

        let (_, mut c) = m.boot(2);
        assert_eq!(len(&c), 6);
        assert_eq!(drain(&mut c), [(1, vec![1; 4]), (2, vec![2; 2])]);
    }

    #[cfg(feature = "async-await")]
    #[test]
    fn next_log_splits_at_boundary() {
        let mut m = Memory::new();
        let (mut p, _) = m.boot(1);
        p.write(&[1; 3]);
        let (mut p, mut c) = m.boot(2);
        p.write(&[2; 2]);

        let max_len = core::num::NonZeroUsize::new(2).unwrap();
        let mut logs = Vec::new();
        while !c.is_empty() {
            let next = c.next_log(max_len, async |id, data| (id.0[0], data.to_vec()));
            let core::task::Poll::Ready(log) = core::pin::pin!(next).poll(
                &mut core::task::Context::from_waker(core::task::Waker::noop()),
            ) else {
                panic!("data is available");
            };
            // Guards against looping forever on empty logs.
            assert!(!log.1.is_empty());
            logs.push(log);
        }
        assert_eq!(logs, [(1, vec![1, 1]), (1, vec![1]), (2, vec![2, 2])]);
    }

    #[test]
    fn silent_firmware_keeps_slots() {
        // A bootloader (9) and an application (1) where only one of them logs.
        for (silent, logging) in [(9, 1), (1, 9)] {
            let mut m = Memory::new();
            for i in 0..SEGMENTS as u8 + 2 {
                m.boot(silent);
                let (mut p, _) = m.boot(logging);
                p.write(&[i]);
            }

            let (_, mut c) = m.boot(silent);
            let data = (0..SEGMENTS as u8 + 2).collect();
            assert_eq!(drain(&mut c), [(logging, data)]);
        }
    }

    #[test]
    fn rebooting_firmware_keeps_older_data() {
        let mut m = Memory::new();
        let (mut p, _) = m.boot(1);
        p.write(&[1]);
        for _ in 0..SEGMENTS + 2 {
            let (mut p, _) = m.boot(2);
            p.write(&[2]);
        }

        let (_, mut c) = m.boot(2);
        assert_eq!(drain(&mut c), [(1, vec![1]), (2, vec![2; SEGMENTS + 2])]);
    }

    #[test]
    fn reset_after_old_data_was_read() {
        let mut m = Memory::new();
        let (mut p, _) = m.boot(1);
        p.write(&[1]);
        let (mut p, mut c) = m.boot(2);
        drain(&mut c);
        p.write(&[2]);

        assert_eq!(m.peek(), Some(Identifier([2; 16])));
        let (_, mut c) = m.boot(2);
        assert_eq!(drain(&mut c), [(2, vec![2])]);
    }

    #[test]
    fn write_wrapping_onto_stale_start() {
        let mut m = Memory::new();
        let (mut p, mut c) = m.boot(1);
        p.write(&[9; 13]);
        drain(&mut c);
        // Ends with `write` at 0, the stale start of the only segment.
        p.write(&[1; 3]);

        let (_, mut c) = m.boot(2);
        assert_eq!(drain(&mut c), [(1, vec![1; 3])]);
    }

    #[test]
    fn identifiers_without_reading() {
        let mut m = Memory::new();
        assert_eq!(m.peek(), None);
        let (mut p, _) = m.boot(1);
        p.write(&[1]);
        let (_, c) = m.boot(2);
        assert_eq!(c.identifier(), &Identifier([2; 16]));
        assert_eq!(m.peek(), Some(Identifier([2; 16])));

        // `init` would reinitialize, so there is nothing to peek at.
        m.set_u32(Memory::start_offset(1), 100);
        assert_eq!(m.peek(), None);
    }

    #[test]
    fn unordered_segment_starts_reinitialize() {
        let mut m = Memory::new();
        for id in 1..=3 {
            let (mut p, _) = m.boot(id);
            p.write(&[id]);
        }
        m.set_u32(Memory::start_offset(1), 2);
        m.set_u32(Memory::start_offset(2), 1);

        assert_eq!(m.peek(), None);
        let (_, mut c) = m.boot(3);
        assert_eq!(drain(&mut c), []);
    }

    #[test]
    fn segment_start_outside_wrapped_buffer_reinitializes() {
        let mut m = Memory::new();
        let (mut p, mut c) = m.boot(1);
        p.write(&[9; 13]);
        drain(&mut c);
        p.write(&[1; 4]);
        let (mut p, _) = m.boot(2);
        p.write(&[2; 2]);
        // Within the readable data when counted from `read`, but past the buffer.
        m.set_u32(Memory::start_offset(1), 17);

        assert_eq!(m.peek(), None);
        let (_, mut c) = m.boot(2);
        assert_eq!(drain(&mut c), []);
    }

    #[test]
    fn invalid_segments_reinitialize() {
        let first = offset_of!(RingBuffer, segments) + offset_of!(Segments, first);
        let last = offset_of!(RingBuffer, segments) + offset_of!(Segments, last);
        let start = Memory::start_offset(1);
        // No segments, indexes outside the table, a start outside the readable
        // data and a start outside the buffer.
        let corruptions: [&dyn Fn(&mut Memory); 5] = [
            &|m| m.0[last] = 0,
            &|m| m.0[first] = SEGMENTS as u8 + 1,
            &|m| m.0[last] = SEGMENTS as u8 + 1,
            &|m| m.set_u32(start, 7),
            &|m| m.set_u32(start, 100),
        ];
        for corrupt in corruptions {
            let mut m = Memory::new();
            let (mut p, _) = m.boot(1);
            p.write(&[1, 1]);
            let (mut p, _) = m.boot(2);
            p.write(&[2]);

            m.0[first] = 0;
            m.0[last] = 2;
            corrupt(&mut m);
            assert_eq!(m.peek(), None);
            let (_, mut c) = m.boot(2);
            assert_eq!(drain(&mut c), []);
        }
    }

    #[test]
    fn stale_oldest_start_is_ignored() {
        // The producer can wrap past the start of the oldest segment.
        for oldest_start in [3, 14] {
            let mut m = Memory::new();
            m.set_state(4, 12, &[(2, oldest_start), (3, 8), (4, 12)]);

            let (_, mut c) = m.boot(5);
            assert_eq!(
                drain(&mut c),
                [(2, vec![4, 5, 6, 7]), (3, vec![8, 9, 10, 11])]
            );
        }
    }

    #[test]
    fn empty_newest_segment_is_dropped() {
        let mut m = Memory::new();
        // Firmware 4 was reset before it logged.
        m.set_state(4, 12, &[(2, 3), (3, 8), (4, 12)]);

        // It still ran last.
        assert_eq!(m.peek(), Some(Identifier([4; 16])));
        let (mut p, mut c) = m.boot(5);
        p.write(&[5]);
        assert_eq!(c.header.segments.count(), 3);
        let expected = [(2, vec![4, 5, 6, 7]), (3, vec![8, 9, 10, 11]), (5, vec![5])];
        assert_eq!(drain(&mut c), expected);
    }

    #[test]
    fn read_at_next_segment_start() {
        let mut m = Memory::new();
        // Segment 1 was read to its end, but not yet dropped.
        m.set_state(3, 12, &[(1, 1), (2, 3), (3, 8), (4, 12)]);

        let (_, mut c) = m.boot(5);
        assert_eq!(
            drain(&mut c),
            [(2, vec![3, 4, 5, 6, 7]), (3, vec![8, 9, 10, 11])]
        );
    }

    #[test]
    fn reset_while_dropping_oldest_segment() {
        let full: Vec<_> = (1..=8).map(|id| (id, id as u32)).collect();
        assert_eq!(full.len(), SEGMENTS);
        // Firmware 9 booted with a full table and moved `read` to the start of
        // segment 2, then a reset hit before or after segment 1 was dropped.
        for segments in [&full[..], &full[1..]] {
            let mut m = Memory::new();
            m.set_state(2, 12, segments);

            assert_eq!(m.peek(), Some(Identifier([8; 16])));
            let (mut p, mut c) = m.boot(9);
            let mut expected: Vec<_> = (2..8).map(|id| (id, vec![id])).collect();
            expected.push((8, vec![8, 9, 10, 11]));
            assert_eq!(drain(&mut c), expected);
            p.write(&[9]);
            assert_eq!(drain(&mut c), [(9, vec![9])]);
        }
    }

    #[test]
    fn unreachable_segments_reinitialize() {
        for segments in [
            // `read` passed the start of segment 2 while segment 1 is kept.
            [(1, 1), (2, 3), (3, 8), (4, 12)],
            // Starts out of order.
            [(1, 1), (2, 8), (3, 5), (4, 12)],
            // A start after `write`.
            [(1, 1), (2, 4), (3, 8), (4, 13)],
        ] {
            let mut m = Memory::new();
            m.set_state(4, 12, &segments);

            assert_eq!(m.peek(), None);
            let (_, mut c) = m.boot(5);
            assert_eq!(drain(&mut c), []);
        }
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
        let (mut p, _) = m.boot(1);
        p.write(&[1, 2]);

        assert_eq!(m.peek(), Some(Identifier([1; 16])));
        let (_, mut c) = m.boot(2);
        assert_eq!(len(&c), 2);
        assert_eq!(drain(&mut c), [(1, vec![1, 2])]);
        assert_eq!(m.peek(), Some(Identifier([2; 16])));
    }

    #[test]
    fn recovers_data_crossing_end() {
        let mut m = Memory::new();
        let (mut p, mut c) = m.boot(1);
        p.write(&[9; 13]);
        drain(&mut c);
        p.write(&[1; 4]);

        let (_, mut c) = m.boot(1);
        assert_eq!(len(&c), 4);
        assert_eq!(drain(&mut c), [(1, vec![1; 4])]);
    }

    #[test]
    fn invalid_indexes_are_repaired() {
        let read = offset_of!(RingBuffer, read);
        let write = offset_of!(RingBuffer, write);
        for (read_value, write_value) in [(100, 2), (0, 100), (100, 100)] {
            let mut m = Memory::new();
            let (mut p, _) = m.boot(1);
            p.write(&[1, 1]);

            m.set_u32(read, read_value);
            m.set_u32(write, write_value);
            assert_eq!(m.peek(), Some(Identifier([1; 16])));
            let (mut p, mut c) = m.boot(1);
            assert_eq!(drain(&mut c), []);
            p.write(&[2]);
            assert_eq!(drain(&mut c), [(1, vec![2])]);
        }
    }

    #[test]
    fn invalid_header_reinitializes() {
        let mut m = Memory::new();
        let (mut p, _) = m.boot(1);
        p.write(&[1, 1]);

        m.set_u32(offset_of!(RingBuffer, header), 0);
        assert_eq!(m.peek(), None);
        let (_, mut c) = m.boot(1);
        assert_eq!(drain(&mut c), []);
    }
}

#[cfg(all(test, loom))]
mod loom_test {
    use super::*;

    extern crate std;
    use loom::{sync::Arc, thread};
    use std::{sync::atomic::AtomicBool, vec::Vec};

    /// A ring buffer shared by a producer and a consumer thread.
    struct Shared {
        header: RingBuffer,
        buf: [UnsafeCell<MaybeUninit<u8>>; LOOM_BUF_LEN],
    }

    // SAFETY: The threads only access it through one Producer and one Consumer.
    unsafe impl Sync for Shared {}

    /// Like [`RingBuffer::split`], but for threads that share the buffer.
    impl Shared {
        fn producer(&self) -> Producer<'_> {
            Producer {
                header: &self.header,
                buf: &self.buf,
            }
        }

        fn consumer(&self) -> Consumer<'_> {
            Consumer {
                header: &self.header,
                buf: &self.buf,
            }
        }
    }

    /// Reads and releases everything that is available.
    fn read_all(c: &mut Consumer<'_>, read: &mut Vec<u8>) {
        let grant = c.read();
        let (buf1, buf2) = grant.bufs();
        read.extend(buf1.iter().chain(buf2));
        grant.release_all();
    }

    #[test]
    fn producer_reuses_released_space() {
        let wrapped = std::sync::Arc::new(AtomicBool::new(false));
        let explored = wrapped.clone();
        loom::model(move || {
            let shared = Arc::new(Shared {
                header: RingBuffer::new(0, 0),
                buf: core::array::from_fn(|_| UnsafeCell::new(MaybeUninit::uninit())),
            });
            let producer = {
                let shared = shared.clone();
                thread::spawn(move || {
                    let mut p = shared.producer();
                    p.write(&[1, 2, 3]);
                    // Wraps around into the space the consumer may have released.
                    p.write(&[4, 5]);
                })
            };

            let mut c = shared.consumer();
            let mut read = Vec::new();
            // With a single read here, loom never explores the producer writing into
            // released space, which the assertion after the model checks.
            for _ in 0..2 {
                read_all(&mut c, &mut read);
            }
            producer.join().unwrap();
            read_all(&mut c, &mut read);

            // The second write is discarded if no space was released yet.
            assert!(read == [1, 2, 3] || read == [1, 2, 3, 4, 5]);
            if read.len() == 5 {
                explored.store(true, Ordering::Relaxed);
            }
        });
        assert!(wrapped.load(Ordering::Relaxed));
    }
}

/// Proofs of what the ring buffer guarantees, run with `cargo kani`.
///
/// WORKAROUND: Kani 0.66 misreads a whole [`Identifier`] at a symbolic index of the segment
/// table, both when comparing and when copying it, see [`kani_misreads_identifiers`]. The
/// proofs therefore only read single bytes of the identifiers in the table, with a symbolic
/// byte index to cover all of them, or compare slots instead of identifiers.
#[cfg(kani)]
mod proofs {
    use super::*;

    /// The largest buffer `RingBuffer::split` allows.
    const MAX_LEN: usize = i32::MAX as usize / 4;

    /// The buffer length in the proofs that need its contents.
    const BUF_LEN: usize = 5;

    /// Any identifier. Only whether two are equal matters, so one byte is enough.
    fn any_identifier() -> Identifier {
        Identifier([kani::any(); 16])
    }

    /// A buffer with any header, indexes and segments.
    fn any_ring_buffer() -> RingBuffer {
        RingBuffer {
            header: kani::any(),
            segments: Segments {
                table: core::array::from_fn(|_| Segment {
                    identifier: any_identifier(),
                    start: AtomicU32::new(kani::any()),
                }),
                first: AtomicU8::new(kani::any()),
                last: AtomicU8::new(kani::any()),
            },
            read: AtomicU32::new(kani::any()),
            write: AtomicU32::new(kani::any()),
            #[cfg(feature = "ecc")]
            _ecc_flush: EccFlush(AtomicU8::new(0)),
        }
    }

    /// Buffer contents with any bytes.
    fn any_buf() -> [UnsafeCell<MaybeUninit<u8>>; BUF_LEN] {
        core::array::from_fn(|_| UnsafeCell::new(MaybeUninit::new(kani::any())))
    }

    fn byte(buf: &[UnsafeCell<MaybeUninit<u8>>], index: usize) -> u8 {
        // SAFETY: `any_buf` initializes all bytes, and no reference to them is alive.
        unsafe { (*buf[index].get()).assume_init() }
    }

    fn load(index: &AtomicU32) -> usize {
        index.load(Ordering::Relaxed) as usize
    }

    /// Proves that `a == b`, when asserted, without the comparison that Kani gets wrong.
    ///
    /// Any byte may be picked, so asserting it proves all bytes are equal. Only use it in
    /// assertions, as it says nothing about the other bytes when it is `false`.
    fn same_identifier(a: &Identifier, b: &Identifier) -> bool {
        let byte = kani::any_where(|&byte: &usize| byte < a.0.len());
        a.0[byte] == b.0[byte]
    }

    /// Kani 0.66 fails this, although it holds. Once Kani passes it, the workaround described
    /// on this module can go.
    #[kani::proof]
    #[kani::unwind(17)]
    #[kani::should_panic]
    fn kani_misreads_identifiers() {
        let rb = any_ring_buffer();
        let slot = kani::any_where(|&slot: &usize| slot < rb.segments.table.len());
        let copy = rb.segments.table[slot].identifier.clone();
        assert!(rb.segments.table[slot].identifier == copy);
    }

    /// The slot of the segment that the data at `index` belongs to, in a valid buffer.
    fn owner(rb: &RingBuffer, index: usize, buf_len: usize) -> usize {
        let ring = Ring(buf_len);
        let read = load(&rb.read);
        let segments = &rb.segments;
        let mut owner = segments.first();
        for n in 1..segments.count() {
            let slot = Segments::RING.add(segments.first(), n);
            if ring.used(read, segments.table[slot].start()) <= ring.used(read, index) {
                owner = slot;
            }
        }
        owner
    }

    /// A valid buffer before recovery, to check the buffer against during and after it.
    #[derive(Clone, Copy)]
    struct Before {
        buf_len: usize,
        read: usize,
        write: usize,
        /// Any byte of data, and the slot and any byte of the identifier it belongs to.
        data: usize,
        slot: usize,
        byte: usize,
        identifier_byte: u8,
    }

    impl Before {
        /// A valid buffer `rb`, with any byte of data in it.
        fn of(rb: &RingBuffer, buf_len: usize) -> Self {
            let data = kani::any_where(|&data| Ring(buf_len).contains(data));
            let slot = owner(rb, data, buf_len);
            let byte = kani::any_where(|&byte: &usize| byte < 16);
            Before {
                buf_len,
                read: load(&rb.read),
                write: load(&rb.write),
                data,
                slot,
                byte,
                identifier_byte: rb.segments.table[slot].identifier.0[byte],
            }
        }

        /// Asserts that `rb` is valid, keeps `write`, has only dropped data from the oldest
        /// end, and labels the data it kept the same.
        fn check(&self, rb: &RingBuffer) {
            let ring = Ring(self.buf_len);
            assert!(rb.header == MAGIC && rb.valid(self.buf_len));
            let read = load(&rb.read);
            assert_eq!(load(&rb.write), self.write);
            assert!(ring.used(self.read, read) <= ring.used(self.read, self.write));
            if ring.used(read, self.data) < ring.used(read, self.write) {
                assert_eq!(owner(rb, self.data, self.buf_len), self.slot);
                let identifier = &rb.segments.table[self.slot].identifier;
                assert_eq!(identifier.0[self.byte], self.identifier_byte);
            }
        }
    }

    /// The buffer that [`resets_keep_labels`] recovers.
    static mut RECOVERING: Option<Before> = None;

    /// The buffer as a reset during [`resets_keep_labels`] would leave it.
    static mut RESET: Option<RingBuffer> = None;

    /// Where a reset may happen during [`resets_keep_labels`], which is at every flush.
    ///
    /// A reset between two flushes leaves the same state as at the flush before, since each
    /// step only makes one store visible. Any one of them may be picked, and the buffer is
    /// copied there to check it once afterwards, as checking at every flush is slow.
    pub(super) fn reset_point(rb: &RingBuffer) {
        // SAFETY: The proofs run on one thread.
        let Some(before) = (unsafe { (&raw const RECOVERING).read() }) else {
            return;
        };
        if kani::any() {
            let copy = RingBuffer {
                header: rb.header,
                segments: Segments {
                    table: core::array::from_fn(|slot| Segment {
                        identifier: Identifier([0; 16]),
                        start: AtomicU32::new(rb.segments.table[slot].start() as u32),
                    }),
                    first: AtomicU8::new(rb.segments.first() as u8),
                    last: AtomicU8::new(rb.segments.last() as u8),
                },
                read: AtomicU32::new(load(&rb.read) as u32),
                write: AtomicU32::new(load(&rb.write) as u32),
                #[cfg(feature = "ecc")]
                _ecc_flush: EccFlush(AtomicU8::new(0)),
            };
            let mut copy = copy;
            // Only this byte of the identifiers is checked.
            copy.segments.table[before.slot].identifier.0[before.byte] =
                rb.segments.table[before.slot].identifier.0[before.byte];
            // SAFETY: As above.
            unsafe { (&raw mut RESET).write(Some(copy)) };
        }
    }

    /// Recovery from any memory gives a valid buffer for the new firmware. From a valid
    /// buffer it keeps `write`, only drops data from the oldest end, and never changes
    /// which firmware the kept data is labeled with.
    #[kani::proof]
    #[kani::unwind(17)]
    fn recovery_keeps_labels() {
        let mut rb = any_ring_buffer();
        // The proofs of `Ring` cover all lengths, the logic only needs to wrap.
        let buf_len = kani::any_where(|&len| 0 < len && len <= BUF_LEN);
        let identifier = any_identifier();
        let before = (rb.header_valid() && rb.valid(buf_len)).then(|| Before::of(&rb, buf_len));

        rb.recover(buf_len, identifier.clone());

        assert!(rb.header_valid() && rb.valid(buf_len));
        assert!(same_identifier(
            &rb.segments.newest().identifier,
            &identifier
        ));
        if let Some(before) = before {
            before.check(&rb);
        }
    }

    /// A reset at any point of recovering a valid buffer leaves a valid buffer with the same
    /// data, except some of the oldest, labeled the same.
    #[kani::proof]
    #[kani::unwind(17)]
    #[kani::solver(cadical)]
    fn resets_keep_labels() {
        let mut rb = any_ring_buffer();
        let buf_len = kani::any_where(|&len| 0 < len && len <= BUF_LEN);
        kani::assume(rb.header_valid() && rb.valid(buf_len));
        let before = Before::of(&rb, buf_len);

        // SAFETY: The proofs run on one thread.
        unsafe { (&raw mut RECOVERING).write(Some(before)) };
        rb.recover(buf_len, any_identifier());
        // SAFETY: As above.
        if let Some(reset) = unsafe { ptr::replace(&raw mut RESET, None) } {
            before.check(&reset);
        }
    }

    /// A grant holds data from `read` that is readable, all from the firmware it names,
    /// and is only empty when there is no data. Releasing it keeps the buffer valid.
    #[kani::proof]
    #[kani::unwind(17)]
    fn grants_hold_labeled_data() {
        let mut rb = any_ring_buffer();
        let buf = any_buf();
        let buf_len = kani::any_where(|&len| 0 < len && len <= BUF_LEN);
        let ring = Ring(buf_len);
        kani::assume(rb.valid(buf_len));
        let (read, write) = (load(&rb.read), load(&rb.write));
        let data = kani::any_where(|&data| ring.contains(data));
        let old_owner = ptr::from_ref(&rb.segments.table[owner(&rb, data, buf_len)].identifier);

        // SAFETY: The buffer is shorter than `i32::MAX / 4`.
        let (_, mut c) = unsafe { rb.split(&buf[..buf_len]) };
        let grant = c.read();
        let (slice1, slice2) = grant.bufs();
        let len = slice1.len() + slice2.len();
        let buf_start: *const u8 = buf.as_ptr().cast();
        assert!(slice1.is_empty() || slice1.as_ptr() == buf_start.wrapping_add(read));
        assert!(slice2.is_empty() || slice2.as_ptr() == buf_start);
        assert!(len <= ring.used(read, write));
        assert_eq!(len == 0, read == write);
        if ring.used(read, data) < len {
            assert!(ptr::eq(grant.identifier(), old_owner));
        }

        grant.release(kani::any());
        assert!(c.header.valid(buf_len));
    }

    /// The producer only writes free space, appends what fits, and keeps the buffer valid.
    #[kani::proof]
    #[kani::unwind(17)]
    fn writes_append_to_free_space() {
        let mut rb = any_ring_buffer();
        let buf = any_buf();
        let buf_len = kani::any_where(|&len| 0 < len && len <= BUF_LEN);
        let ring = Ring(buf_len);
        kani::assume(rb.valid(buf_len));
        let (read, write) = (load(&rb.read), load(&rb.write));
        let data: [u8; BUF_LEN] = kani::any();
        let len = kani::any_where(|&len| len <= BUF_LEN);
        let old = kani::any_where(|&old| {
            ring.contains(old) && ring.used(read, old) < ring.used(read, write)
        });
        let old_byte = byte(&buf, old);

        // SAFETY: The buffer is shorter than `i32::MAX / 4`.
        let (mut p, c) = unsafe { rb.split(&buf[..buf_len]) };
        p.write(&data[..len]);

        let written = len.min(ring.free(read, write));
        assert_eq!(load(&c.header.write), ring.add(write, written));
        assert!(c.header.valid(buf_len));
        assert_eq!(byte(&buf, old), old_byte);
        let new = kani::any_where(|&new| new < written);
        assert_eq!(byte(&buf, ring.add(write, new)), data[new]);
    }

    /// A ring of any length, with an index in it.
    fn any_ring() -> (Ring, usize) {
        let len = kani::any_where(|&len| 0 < len && len <= MAX_LEN);
        (Ring(len), kani::any_where(|&index| index < len))
    }

    #[kani::proof]
    fn ring_used_and_free() {
        let (ring, start) = any_ring();
        let end = kani::any_where(|&end| ring.contains(end));
        let used = ring.used(start, end);
        assert!(used < ring.0);
        assert_eq!(used + ring.free(start, end), ring.0 - 1);
        assert_eq!(ring.add(start, used), end);
    }

    #[kani::proof]
    fn ring_add_and_sub() {
        let (ring, index) = any_ring();
        let n = kani::any_where(|&n| n < ring.0);
        assert!(ring.contains(ring.add(index, n)));
        assert!(ring.contains(ring.sub(index, n)));
        assert_eq!(ring.sub(ring.add(index, n), n), index);
    }

    #[kani::proof]
    fn ring_ranges() {
        let (ring, start) = any_ring();
        let n = kani::any_where(|&n| n < ring.0);
        let (first, second) = ring.ranges(start, n);
        assert_eq!(first.start, start);
        assert!(first.end <= ring.0 && second.end <= ring.0);
        assert_eq!(first.len() + second.len(), n);
        // The second range continues where the first one wraps.
        assert!(second.is_empty() || (first.end == ring.0 && second.start == 0));
        // They never overlap, also not when wrapping onto `start`.
        assert!(second.end <= first.start);
    }
}
