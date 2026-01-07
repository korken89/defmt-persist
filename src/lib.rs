#![no_std]
#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

use core::mem::{align_of, size_of};
use core::sync::atomic::{AtomicBool, Ordering};
use ring_buffer::RingBuffer;
#[cfg(feature = "qemu-test")]
pub use ring_buffer::offsets;
pub use ring_buffer::{Consumer, GrantR};

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

/// Initialize the logger.
///
/// This reads the buffer region from the linker symbols `__defmt_persist_start` and
/// `__defmt_persist_end`. Define these in your linker script to reserve memory for
/// the persist buffer.
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
pub fn init() -> Result<Consumer<'static>, InitError> {
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
    let (p, c) = unsafe { RingBuffer::recover_or_reinitialize(memory) };

    // SAFETY: The atomic swap guarantees this is called only once.
    unsafe { logger::LOGGER_STATE.initialize(p) };

    Ok(c)
}
