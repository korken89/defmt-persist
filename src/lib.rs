#![no_std]
#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

use core::mem::{align_of, size_of};
use core::sync::atomic::{AtomicBool, Ordering};
use ring_buffer::RingBuffer;
pub use ring_buffer::{Consumer, GrantR};

#[cfg(feature = "async-await")]
pub(crate) mod atomic_waker;
pub(crate) mod logger;
mod ring_buffer;

/// Initialize the logger.
///
/// This reads the buffer region from the linker symbols `__defmt_persist_start` and
/// `__defmt_persist_end`. Define these in your linker script to reserve memory for
/// the persist buffer.
///
/// Returns `None` if:
/// - Already initialized (called more than once)
/// - Memory region is not properly aligned for [`RingBuffer`]
/// - Memory region is too small to hold the [`RingBuffer`] header plus data
/// - Buffer size would overflow pointer arithmetic
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
    // SAFETY: These symbols are provided by the linker script and point to a reserved memory region.
    unsafe extern "C" {
        static __defmt_persist_start: u8;
        static __defmt_persist_end: u8;
    }

    static INITIALIZED: AtomicBool = AtomicBool::new(false);

    if INITIALIZED.swap(true, Ordering::SeqCst) {
        return None;
    }

    let start = (&raw const __defmt_persist_start).expose_provenance();
    let end = (&raw const __defmt_persist_end).expose_provenance();
    let memory = start..end;

    // Validate alignment and size requirements.
    if !memory.start.is_multiple_of(align_of::<RingBuffer>()) {
        return None;
    }
    if memory.len() <= size_of::<RingBuffer>() {
        return None;
    }
    // Ensure buffer size doesn't overflow pointer arithmetic.
    let buf_len = memory.len() - size_of::<RingBuffer>();
    if buf_len >= isize::MAX as usize / 4 {
        return None;
    }

    // SAFETY: Linker symbols provide the memory region. The atomic swap above
    // guarantees this code runs exactly once, ensuring exclusive ownership.
    // Alignment and size are validated above.
    let (p, c) = unsafe { RingBuffer::recover_or_reinitialize(memory) };

    // SAFETY: The atomic swap guarantees this is called only once.
    unsafe { logger::LOGGER_STATE.initialize(p) };

    Some(c)
}
