#![no_std]
#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

use core::mem::{align_of, size_of};
use core::ops::Range;
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
    /// The identifier stored by the previous run.
    ///
    /// The recovered logs can also contain logs from earlier firmwares. Use
    /// [`GrantR::identifier`] to tell them apart.
    pub recovered_identifier: Option<Identifier>,
}

static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Initialize the logger.
///
/// This reads the buffer region from the linker symbols `__defmt_persist_start` and
/// `__defmt_persist_end`. Define these in your linker script to reserve memory for
/// the persist buffer.
///
/// # Identifier
///
/// Use the identifier to detect if recovered logs were produced by a different firmware
/// version. The returned [`ConsumerAndMetadata`] contains both:
/// - `recovered_identifier`: The identifier from before this call (previous firmware)
/// - `consumer.identifier()`: The new identifier set by this call (current firmware)
///
/// The buffer keeps track of which firmware wrote which data, also across several resets and
/// when both a bootloader and an application log. Every [`GrantR`] only holds data from one
/// firmware, see [`GrantR::identifier`]. Up to 8 segments are tracked, and a new one starts each
/// time a different firmware logs, so a bootloader and an application that both log start two
/// per boot. When more are needed, the unread data of the oldest segment is dropped.
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
pub fn init(new_identifier: Identifier) -> Result<ConsumerAndMetadata<'static>, InitError> {
    if INITIALIZED.swap(true, Ordering::SeqCst) {
        return Err(InitError::AlreadyInitialized);
    }

    let memory = get_persist_memory_from_symbols()?;

    // SAFETY:
    // - Linker symbols provide the memory region.
    // - The atomic swap above guarantees this code runs exactly once, ensuring exclusive ownership.
    // - Alignment and size are validated by get_persist_memory_from_symbols.
    let (p, c, old_identifier) =
        unsafe { RingBuffer::recover_or_reinitialize(memory, new_identifier) };

    let recovered_logs_len = c.len();

    // SAFETY: The atomic swap guarantees this is called only once.
    unsafe { logger::LOGGER_STATE.initialize(p) };

    Ok(ConsumerAndMetadata {
        consumer: c,
        recovered_logs_len,
        recovered_identifier: old_identifier,
    })
}

/// Read the identifier without going through initialization.
///
/// If the header indicates that the identifier is garbage, `None` is
/// returned.
///
/// # Errors
///
/// Returns an error if:
/// - [`InitError::AlreadyInitialized`]: Called after [`init`]
/// - [`InitError::BadAlignment`]: Memory region is not properly aligned
/// - [`InitError::TooSmall`]: Memory region is too small for the header plus data
/// - [`InitError::TooLarge`]: Buffer size would overflow pointer arithmetic
pub fn peek_identifier() -> Result<Option<Identifier>, InitError> {
    let memory = get_persist_memory_from_symbols()?;

    // Block `init` from taking the memory for the duration of this
    // function. The mechanism is a bit clunky, but if someone managed
    // to concurrently call peek and init, this will produce a
    // surprising error instead of memory unsafety.
    if INITIALIZED.swap(true, Ordering::SeqCst) {
        return Err(InitError::AlreadyInitialized);
    }

    // SAFETY:
    // - Linker symbols provide the memory region.
    // - The atomic swap above guarantees this code has exclusive ownership.
    // - Alignment and size are validated by get_persist_memory_from_symbols.
    let v = unsafe { RingBuffer::peek_identifier(memory) };

    INITIALIZED.store(false, Ordering::SeqCst);

    Ok(v)
}

fn get_persist_memory_from_symbols() -> Result<Range<usize>, InitError> {
    // SAFETY: These symbols are provided by the linker script and point to a reserved memory region.
    unsafe extern "C" {
        static __defmt_persist_start: u8;
        static __defmt_persist_end: u8;
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

    Ok(memory)
}

/// Logger memory that may need special treatment.
pub struct Memory {
    /// User-configured persist memory.
    pub persist: Range<usize>,
    /// The place where RTT clients discover available channels and
    /// where the ring buffer bookkeeping is done.
    #[cfg(feature = "rtt")]
    pub rtt_header: Range<usize>,
    /// The channel name for the single RTT channel, pointed to by the header.
    #[cfg(feature = "rtt")]
    pub rtt_channel_name: Range<usize>,
    /// RTT log data resides here.
    #[cfg(feature = "rtt")]
    pub rtt_buffer: Range<usize>,
}

/// Memory allocations associated with the logger that may need special treatment.
///
/// Any of these that are placed in memory with caching support should
/// be configured as non-cacheable before calling `init` for the logger
/// to work properly.
///
/// The configuration of this is chip-dependent, but will typically
/// involve configuring an MPU.
pub fn memory() -> Result<Memory, InitError> {
    Ok(Memory {
        persist: get_persist_memory_from_symbols()?,
        #[cfg(feature = "rtt")]
        rtt_header: logger::rtt::rtt_header_memory(),
        #[cfg(feature = "rtt")]
        rtt_channel_name: logger::rtt::rtt_channel_name_memory(),
        #[cfg(feature = "rtt")]
        rtt_buffer: logger::rtt::rtt_buffer_memory(),
    })
}
