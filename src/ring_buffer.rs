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
    /// The firmwares that wrote the buffer contents, oldest first.
    ///
    /// Segments `first..last` are in use, wrapping around the table. The data
    /// of each segment ends where the next one starts, and the last one ends
    /// at `write`. The RingBuffer always guarantees that `first..last` holds
    /// 1 to [`SEGMENTS`] segments, and that the start of every segment except
    /// the first lies between `read` and `write`, in order.
    segments: [Segment; SEGMENTS],
    /// Index of the oldest segment, modulo [`SEGMENTS`].
    first: AtomicU32,
    /// Index after the newest segment, modulo [`SEGMENTS`].
    last: AtomicU32,
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

/// Data written by one firmware.
#[repr(C)]
struct Segment {
    identifier: Identifier,
    start: AtomicU32,
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

/// Bytes from `from` to `to`, going forward in a ring of `len` bytes.
fn ring_distance(from: usize, to: usize, len: usize) -> usize {
    if to >= from {
        to - from
    } else {
        len - from + to
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
// - After initialization only the Consumer writes header.first, and the segment
//   identifiers and starts are not modified
// - The UnsafeCell slice is only accessed through methods that maintain the SPSC invariant
unsafe impl Send for Consumer<'_> {}

/// Value used to indicate that the queue is initialized.
///
/// Replace this if the layout or field semantics change in a backwards-incompatible way.
/// The `ecc` layout uses a different magic to force reinitialization when switching.
#[cfg(not(feature = "ecc"))]
const MAGIC: u128 = 0xc543_96ee_312f_9601_4ab4_7c0d_9bb9_774e;
#[cfg(feature = "ecc")]
const MAGIC: u128 = 0x7d25_1d3a_9333_326d_60de_302b_166d_c530;

/// Number of segments, where a new one starts each time a different firmware logs.
///
/// A power of two, so segment indices stay consistent when they wrap around `u32`.
const SEGMENTS: usize = 8;
const _: () = assert!(SEGMENTS.is_power_of_two());

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
            segments: core::array::from_fn(|_| Segment {
                identifier: Identifier([0; 16]),
                start: AtomicU32::new(0),
            }),
            first: AtomicU32::new(0),
            last: AtomicU32::new(1),
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
        let old_identifier = if v.header_valid() && v.repair(buf_len) {
            let old_identifier = v.newest_segment().identifier.read_volatile();
            if old_identifier != new_identifier {
                v.push_segment(new_identifier);
            }
            v.drop_read_segments();
            Some(old_identifier)
        } else {
            v.set_header(0);
            v.read.store(0, Ordering::Relaxed);
            v.write.store(0, Ordering::Relaxed);
            v.first.store(0, Ordering::Relaxed);
            v.last.store(1, Ordering::Relaxed);
            v.segments[0].identifier.write_volatile(new_identifier);

            // Everything must be in memory before the header claims it is
            // valid, otherwise a reset in between recovers garbage.
            v.set_header(MAGIC);
            None
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
        self.segments_valid(read, write, buf_len)
    }

    /// The `read` and `write` indexes that [`Self::repair`] brings the buffer to.
    fn repaired_indexes(&self, buf_len: usize) -> (usize, usize) {
        // Even if the header promises to keep the contract, we don't
        // trust it for the safety of our pointer offsets.
        let read = self.read.load(Ordering::Relaxed) as usize;
        let write = self.write.load(Ordering::Relaxed) as usize;
        match (read < buf_len, write < buf_len) {
            (true, true) => (read, write),
            (true, false) => (read, read),
            (false, true) => (write, write),
            (false, false) => (0, 0),
        }
    }

    /// Returns `true` if the segments describe the data between `read` and `write`.
    fn segments_valid(&self, read: usize, write: usize, buf_len: usize) -> bool {
        // The consumer trusts the segment starts as the end of a read, so
        // they must lie within the readable data.
        let readable = ring_distance(read, write, buf_len);
        let first = self.first.load(Ordering::Relaxed);
        let count = self.last.load(Ordering::Relaxed).wrapping_sub(first);
        if !(1..=SEGMENTS as u32).contains(&count) {
            return false;
        }
        let mut previous = 0;
        (1..count).all(|i| {
            let start = self
                .segment(first.wrapping_add(i))
                .start
                .load(Ordering::Relaxed) as usize;
            let distance = ring_distance(read, start, buf_len);
            let ok = start < buf_len && previous <= distance && distance <= readable;
            previous = distance;
            ok
        })
    }

    fn segment(&self, index: u32) -> &Segment {
        &self.segments[index as usize % SEGMENTS]
    }

    fn newest_segment(&mut self) -> &mut Segment {
        let index = self.last.load(Ordering::Relaxed).wrapping_sub(1);
        &mut self.segments[index as usize % SEGMENTS]
    }

    /// Starts a new segment at `write`, for data from the firmware `identifier`.
    ///
    /// Each step is a single store, so a reset at any point leaves a valid state.
    fn push_segment(&mut self, identifier: Identifier) {
        let write = self.write.load(Ordering::Relaxed);
        let first = self.first.load(Ordering::Relaxed);
        let mut last = self.last.load(Ordering::Relaxed);

        if last.wrapping_sub(first) > 1
            && self.newest_segment().start.load(Ordering::Relaxed) == write
        {
            // The previous firmware did not log, so its segment can be reused.
            last = last.wrapping_sub(1);
            self.last.store(last, Ordering::Relaxed);
            self.flush();
            if self.newest_segment().identifier.read_volatile() == identifier {
                // The firmware before it is this one, so its segment continues.
                return;
            }
        }
        if last.wrapping_sub(first) == SEGMENTS as u32 {
            // The table is full, drop the data of the oldest firmware.
            let second = self
                .segment(first.wrapping_add(1))
                .start
                .load(Ordering::Relaxed);
            self.read.store(second, Ordering::Relaxed);
            self.flush();
            self.first.store(first.wrapping_add(1), Ordering::Relaxed);
            self.flush();
        }

        // The slot at `last` is unused until `last` moves past it.
        let segment = &mut self.segments[last as usize % SEGMENTS];
        segment.identifier.write_volatile(identifier);
        segment.start.store(write, Ordering::Relaxed);
        self.flush();
        self.last.store(last.wrapping_add(1), Ordering::Relaxed);
        self.flush();
    }

    /// Drops the oldest segments that have been read completely, returning the
    /// index of the first remaining one.
    fn drop_read_segments(&self) -> u32 {
        let read = self.read.load(Ordering::Relaxed);
        let last = self.last.load(Ordering::Relaxed);
        let mut first = self.first.load(Ordering::Relaxed);
        while last.wrapping_sub(first) > 1
            && self
                .segment(first.wrapping_add(1))
                .start
                .load(Ordering::Relaxed)
                == read
        {
            first = first.wrapping_add(1);
            // `read` must reach memory before `first`, also across a reset.
            self.flush();
            self.first.store(first, Ordering::Relaxed);
            self.flush_ecc();
        }
        first
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
        let buf_len = memory.len() - size_of::<RingBuffer>();
        // SAFETY: The safety contract is reexported.
        let v = unsafe { Self::from_memory(&memory) };
        let (read, write) = v.repaired_indexes(buf_len);
        (v.header_valid() && v.segments_valid(read, write, buf_len))
            .then(|| v.newest_segment().identifier.read_volatile())
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

impl<'c> Consumer<'c> {
    /// Access the current identifier (as set by [`init`](crate::init)).
    ///
    /// Recovered logs may have been written by an older firmware. Use
    /// [`GrantR::identifier`] to get the identifier of the data being read.
    pub fn identifier(&self) -> &'c Identifier {
        let last = self.header.last.load(Ordering::Relaxed);
        &self.header.segment(last.wrapping_sub(1)).identifier
    }

    /// Number of bytes available to read, including data from an older firmware.
    pub(crate) fn len(&self) -> usize {
        let write = self.header.write.load(Ordering::Acquire) as usize;
        let read = self.header.read.load(Ordering::Relaxed) as usize;
        ring_distance(read, write, self.buf.len())
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
        let first = header.drop_read_segments();
        let last = header.last.load(Ordering::Relaxed);
        let next = first.wrapping_add(1);
        let end = if next == last {
            write
        } else {
            header.segment(next).start.load(Ordering::Relaxed) as usize
        };
        let identifier = &header.segment(first).identifier;
        let buf: *mut u8 = self.buf.as_ptr().cast_mut().cast();

        let (len1, len2) = if end < read {
            (self.buf.len() - read, end)
        } else {
            (end - read, 0)
        };

        // SAFETY:
        // For `slice::from_raw_parts`:
        // - Non-null, valid, aligned: it is a sub-slice of `buf`,
        //   relying on the invariants on `read`, `write` and `segments`.
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
        //   before constructing a Consumer. end - read fits if end >= read,
        //   which holds in the cases we use it.
        // - entire memory range inside the same allocation: read < len, so the
        //   offset remains in the buffer's allocation.
        let slice1 = unsafe { slice::from_raw_parts(buf.add(read), len1) };
        // SAFETY:
        // For `slice::from_raw_parts`:
        // - Non-null, valid, aligned: it is a sub-slice of `buf`,
        //   relying on the invariants on `read`, `write` and `segments`.
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
        //   before constructing a Consumer. end - read fits if end >= read,
        //   which holds in the cases we use it.
        // - entire memory range inside the same allocation: read < len, so the
        //   offset remains in the buffer's allocation.
        let slice2 = unsafe { slice::from_raw_parts(buf, len2) };
        GrantR {
            consumer: self,
            slice1,
            slice2,
            original_read: read,
            identifier,
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

    /// Identifies the firmware that wrote the bytes in this grant.
    ///
    /// The reference stays valid after the grant is released.
    #[inline]
    pub fn identifier(&self) -> &'c Identifier {
        self.identifier
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

        /// Offset of the start of segment slot `index`.
        fn start_offset(index: usize) -> usize {
            offset_of!(RingBuffer, segments)
                + index * size_of::<Segment>()
                + offset_of!(Segment, start)
        }

        fn set_u32(&mut self, offset: usize, value: u32) {
            self.0[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
        }
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
        let (mut p, _, recovered) = m.boot(1);
        assert_eq!(recovered, None);
        p.write(&[1, 1]);

        let (mut p, c, recovered) = m.boot(2);
        assert_eq!(recovered, Some(Identifier([1; 16])));
        assert_eq!(c.len(), 2);
        p.write(&[2]);

        let (_, mut c, recovered) = m.boot(2);
        assert_eq!(recovered, Some(Identifier([2; 16])));
        // The identifier outlives the grant.
        let oldest = c.read().identifier();
        assert_eq!(drain(&mut c), [(1, vec![1, 1]), (2, vec![2])]);
        assert_eq!(oldest, &Identifier([1; 16]));
    }

    #[test]
    fn bootloader_and_application_both_log() {
        let mut m = Memory::new();
        for _ in 0..2 {
            let (mut p, _, _) = m.boot(9);
            p.write(&[9]);
            let (mut p, _, _) = m.boot(1);
            p.write(&[1]);
        }

        let (_, mut c, recovered) = m.boot(9);
        assert_eq!(recovered, Some(Identifier([1; 16])));
        let expected = [(9, vec![9]), (1, vec![1]), (9, vec![9]), (1, vec![1])];
        assert_eq!(drain(&mut c), expected);
    }

    #[test]
    fn full_table_drops_oldest_data() {
        let mut m = Memory::new();
        for id in 1..=SEGMENTS as u8 + 1 {
            let (mut p, _, _) = m.boot(id);
            p.write(&[id]);
        }

        let (_, mut c, _) = m.boot(SEGMENTS as u8 + 1);
        let expected: Vec<_> = (2..=SEGMENTS as u8 + 1).map(|id| (id, vec![id])).collect();
        assert_eq!(drain(&mut c), expected);
    }

    #[test]
    fn firmwares_without_logs_keep_old_data() {
        let mut m = Memory::new();
        let (mut p, _, _) = m.boot(1);
        p.write(&[1]);
        for id in (2..=3).cycle().take(4 * SEGMENTS) {
            m.boot(id);
        }

        let (mut p, mut c, recovered) = m.boot(4);
        assert_eq!(recovered, Some(Identifier([3; 16])));
        assert_eq!(drain(&mut c), [(1, vec![1])]);
        p.write(&[4]);
        assert_eq!(drain(&mut c), [(4, vec![4])]);
    }

    #[test]
    fn partially_read_old_data_keeps_identifier() {
        let mut m = Memory::new();
        let (mut p, _, _) = m.boot(1);
        p.write(&[1, 1, 1]);

        let (mut p, mut c, _) = m.boot(2);
        c.read().release(1);
        p.write(&[2]);

        let (_, mut c, _) = m.boot(2);
        assert_eq!(drain(&mut c), [(1, vec![1, 1]), (2, vec![2])]);
    }

    #[test]
    fn old_data_crossing_end() {
        let mut m = Memory::new();
        let (mut p, mut c, _) = m.boot(1);
        p.write(&[9; 13]);
        drain(&mut c);
        p.write(&[1; 4]);

        let (mut p, _, _) = m.boot(2);
        p.write(&[2; 2]);

        let (_, mut c, _) = m.boot(2);
        assert_eq!(c.len(), 6);
        assert_eq!(drain(&mut c), [(1, vec![1; 4]), (2, vec![2; 2])]);
    }

    #[test]
    fn silent_firmware_keeps_slots() {
        // A bootloader (9) and an application (1) where only one of them logs.
        for (silent, logging) in [(9, 1), (1, 9)] {
            let mut m = Memory::new();
            for i in 0..SEGMENTS as u8 + 2 {
                m.boot(silent);
                let (mut p, _, _) = m.boot(logging);
                p.write(&[i]);
            }

            let (_, mut c, _) = m.boot(silent);
            let data = (0..SEGMENTS as u8 + 2).collect();
            assert_eq!(drain(&mut c), [(logging, data)]);
        }
    }

    #[test]
    fn rebooting_firmware_keeps_older_data() {
        let mut m = Memory::new();
        let (mut p, _, _) = m.boot(1);
        p.write(&[1]);
        for _ in 0..SEGMENTS + 2 {
            let (mut p, _, _) = m.boot(2);
            p.write(&[2]);
        }

        let (_, mut c, _) = m.boot(2);
        assert_eq!(drain(&mut c), [(1, vec![1]), (2, vec![2; SEGMENTS + 2])]);
    }

    #[test]
    fn reset_after_old_data_was_read() {
        let mut m = Memory::new();
        let (mut p, _, _) = m.boot(1);
        p.write(&[1]);
        let (mut p, mut c, _) = m.boot(2);
        drain(&mut c);
        p.write(&[2]);

        let (_, mut c, recovered) = m.boot(2);
        assert_eq!(recovered, Some(Identifier([2; 16])));
        assert_eq!(drain(&mut c), [(2, vec![2])]);
    }

    #[test]
    fn write_wrapping_onto_stale_start() {
        let mut m = Memory::new();
        let (mut p, mut c, _) = m.boot(1);
        p.write(&[9; 13]);
        drain(&mut c);
        // Ends with `write` at 0, the stale start of the only segment.
        p.write(&[1; 3]);

        let (_, mut c, _) = m.boot(2);
        assert_eq!(drain(&mut c), [(1, vec![1; 3])]);
    }

    #[test]
    fn segment_counters_wrap() {
        let mut m = Memory::new();
        let (mut p, _, _) = m.boot(1);
        p.write(&[1]);
        // Keeps slot 0 as the only segment.
        let first = u32::MAX - SEGMENTS as u32 + 1;
        m.set_u32(offset_of!(RingBuffer, first), first);
        m.set_u32(offset_of!(RingBuffer, last), first + 1);

        for id in 2..=SEGMENTS as u8 + 1 {
            let (mut p, _, _) = m.boot(id);
            p.write(&[id]);
        }
        let (_, mut c, _) = m.boot(SEGMENTS as u8 + 1);
        let expected: Vec<_> = (2..=SEGMENTS as u8 + 1).map(|id| (id, vec![id])).collect();
        assert_eq!(drain(&mut c), expected);
    }

    #[test]
    fn identifiers_without_reading() {
        let mut m = Memory::new();
        assert_eq!(m.peek(), None);
        let (mut p, _, _) = m.boot(1);
        p.write(&[1]);
        let (_, c, _) = m.boot(2);
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
            let (mut p, _, _) = m.boot(id);
            p.write(&[id]);
        }
        m.set_u32(Memory::start_offset(1), 2);
        m.set_u32(Memory::start_offset(2), 1);

        let (_, mut c, recovered) = m.boot(3);
        assert_eq!(recovered, None);
        assert_eq!(drain(&mut c), []);
    }

    #[test]
    fn segment_start_outside_wrapped_buffer_reinitializes() {
        let mut m = Memory::new();
        let (mut p, mut c, _) = m.boot(1);
        p.write(&[9; 13]);
        drain(&mut c);
        p.write(&[1; 4]);
        let (mut p, _, _) = m.boot(2);
        p.write(&[2; 2]);
        // Within the readable data when counted from `read`, but past the buffer.
        m.set_u32(Memory::start_offset(1), 17);

        let (_, mut c, recovered) = m.boot(2);
        assert_eq!(recovered, None);
        assert_eq!(drain(&mut c), []);
    }

    #[test]
    fn invalid_segments_reinitialize() {
        let first = offset_of!(RingBuffer, first);
        let last = offset_of!(RingBuffer, last);
        let start = Memory::start_offset(1);
        // No segments, too many segments, a start outside the readable data
        // and a start outside the buffer.
        for (offset, value) in [
            (last, 0),
            (last, SEGMENTS as u32 + 1),
            (start, 7),
            (start, 100),
        ] {
            let mut m = Memory::new();
            let (mut p, _, _) = m.boot(1);
            p.write(&[1, 1]);
            let (mut p, _, _) = m.boot(2);
            p.write(&[2]);

            m.set_u32(first, 0);
            m.set_u32(last, 2);
            m.set_u32(offset, value);
            let (_, mut c, recovered) = m.boot(2);
            assert_eq!(recovered, None);
            assert_eq!(drain(&mut c), []);
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
        assert_eq!(drain(&mut c), [(1, vec![1, 2])]);
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
        assert_eq!(drain(&mut c), [(1, vec![1; 4])]);
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
            assert_eq!(drain(&mut c), []);
            p.write(&[2]);
            assert_eq!(drain(&mut c), [(1, vec![2])]);
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
        assert_eq!(drain(&mut c), []);
    }
}
