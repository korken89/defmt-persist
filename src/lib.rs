#![no_std]
#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

use core::sync::atomic::{AtomicBool, Ordering};
use ring_buffer::RingBuffer;
pub use ring_buffer::{Consumer, GrantR};

#[cfg(feature = "async-await")]
pub(crate) mod atomic_waker;
pub(crate) mod logger;
mod ring_buffer;

// Linker symbols defining the persist buffer region.
// Must be defined in the user's linker script.
// SAFETY: These symbols are provided by the linker script and point to a reserved memory region.
unsafe extern "C" {
    static __defmt_persist_start: u8;
    static __defmt_persist_end: u8;
}

/// Tracks whether [`init`] has been called.
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Initialize the logger.
///
/// This reads the buffer region from the linker symbols `__defmt_persist_start` and
/// `__defmt_persist_end`. Define these in your linker script to reserve memory for
/// the persist buffer.
///
/// # Safety considerations
///
/// The linker symbols must define a valid memory region that is not used for any
/// other purpose. It is safe for both a bootloader and application to call this,
/// provided the bootloader terminates before the application starts.
///
/// Corrupt memory may be accepted as valid. While index bounds are validated,
/// the data content is not. Treat recovered logs as untrusted external input.
pub fn init() -> Option<Consumer<'static>> {
    if INITIALIZED.swap(true, Ordering::SeqCst) {
        return None;
    }

    let start = (&raw const __defmt_persist_start).expose_provenance();
    let end = (&raw const __defmt_persist_end).expose_provenance();

    // SAFETY: Linker symbols provide the memory region. The atomic swap above
    // guarantees this code runs exactly once, ensuring exclusive ownership.
    let (p, c) = unsafe {
        RingBuffer::recover_or_reinitialize(
            start..end,
            #[cfg(feature = "async-await")]
            &logger::WAKER,
        )
    };

    // SAFETY: The atomic swap guarantees this is called only once.
    unsafe { logger::LOGGER_STATE.initialize(p) };

    Some(c)
}
