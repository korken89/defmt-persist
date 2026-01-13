#![no_std]
#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

use core::mem::{align_of, size_of};
use core::sync::atomic::{AtomicBool, Ordering};
use ring_buffer::RingBuffer;
#[cfg(feature = "qemu-test")]
pub use ring_buffer::offsets;
pub use ring_buffer::{Consumer, GrantR, Identifier};

#[cfg(feature = "async-await")]
pub(crate) mod atomic_waker;
pub(crate) mod logger;
mod ring_buffer;

/// Error returned by [`init`] when initialization fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, defmt::Format)]
pub enum InitError {
    /// [`init`] has already been called.
    AlreadyInitialized,
    /// Memory region is not properly aligned for the ring buffer header.
    BadAlignment,
    /// Memory region is too small to hold the ring buffer header plus data.
    TooSmall,
    /// Buffer size would overflow pointer arithmetic.
    TooLarge,
}

/// Holds the log reader and some additional information from initialization.
pub struct ConsumerAndMetadata<'a> {
    /// Reads logs from the buffer.
    pub consumer: Consumer<'a>,
    /// Number of bytes that were not from the current run.
    ///
    /// If the recovered logs were produced by a different firmware,
    /// different decoders need to be used. This field helps identify the
    /// data that was definitely produced by the current firmware.
    pub recovered_logs_len: usize,
    /// The identifier for the firmware that generated the recovered logs.
    pub recovered_identifier: Identifier,
}

/// Initialize the logger.
///
/// This reads the buffer region from the linker symbols `__defmt_persist_start` and
/// `__defmt_persist_end`. Define these in your linker script to reserve memory for
/// the persist buffer.
///
/// # Identifier
///
/// The closure `f` receives the previous [`Identifier`] and returns a new one to store.
/// Use the identifier to detect if recovered logs were produced by a different firmware
/// version. The returned [`ConsumerAndMetadata`] contains both:
/// - `recovered_identifier`: The identifier from before this call (previous firmware)
/// - `consumer.identifier()`: The new identifier set by the closure (current firmware)
///
/// A common approach is to hash the firmware flash content using a simple hash function
/// like FNV-1a. Use a hash with enough output bits to avoid collisions between firmware
/// versions.
///
/// # Errors
///
/// Returns an error if:
/// - [`InitError::AlreadyInitialized`]: Called more than once
/// - [`InitError::BadAlignment`]: Memory region is not properly aligned
/// - [`InitError::TooSmall`]: Memory region is too small for the header plus data
/// - [`InitError::TooLarge`]: Buffer size would overflow pointer arithmetic
///
/// # Safety considerations
///
/// The linker symbols must define a valid memory region that is not used for any
/// other purpose. It is safe for both a bootloader and application to call this,
/// provided the bootloader terminates before the application starts.
///
/// Corrupt memory may be accepted as valid. While index bounds are validated,
/// the data content is not. Treat recovered logs as untrusted external input.
pub fn init(
    f: impl FnOnce(&Identifier) -> Identifier,
) -> Result<ConsumerAndMetadata<'static>, InitError> {
    // SAFETY: These symbols are provided by the linker script and point to a reserved memory region.
    unsafe extern "C" {
        static __defmt_persist_start: u8;
        static __defmt_persist_end: u8;
    }

    static INITIALIZED: AtomicBool = AtomicBool::new(false);

    if INITIALIZED.swap(true, Ordering::SeqCst) {
        return Err(InitError::AlreadyInitialized);
    }

    let start = (&raw const __defmt_persist_start).expose_provenance();
    let end = (&raw const __defmt_persist_end).expose_provenance();
    let memory = start..end;

    if !memory.start.is_multiple_of(align_of::<RingBuffer>()) {
        return Err(InitError::BadAlignment);
    }
    if memory.len() <= size_of::<RingBuffer>() {
        return Err(InitError::TooSmall);
    }
    let buf_len = memory.len() - size_of::<RingBuffer>();
    if buf_len >= i32::MAX as usize / 4 {
        return Err(InitError::TooLarge);
    }

    // SAFETY:
    // - Linker symbols provide the memory region.
    // - The atomic swap above guarantees this code runs exactly once, ensuring exclusive ownership.
    // - Alignment and size are validated above.
    let (p, mut c, identifier) = unsafe { RingBuffer::recover_or_reinitialize(memory) };

    // SAFETY: The atomic swap guarantees this is called only once.
    unsafe { logger::LOGGER_STATE.initialize(p) };

    let recovered_logs_len = {
        let grant = c.read();
        let (buf1, buf2) = grant.bufs();
        buf1.len() + buf2.len()
    };

    // SAFETY: `identifier` points to a valid, aligned field within the RingBuffer.
    let old_identifier = unsafe { identifier.read_volatile() };
    let new_identifier = f(&old_identifier);
    // SAFETY: `identifier` points to a valid, aligned field within the RingBuffer.
    unsafe { identifier.write_volatile(new_identifier) };
    c.header.flush_ecc();

    Ok(ConsumerAndMetadata {
        consumer: c,
        recovered_logs_len,
        recovered_identifier: old_identifier,
    })
}
