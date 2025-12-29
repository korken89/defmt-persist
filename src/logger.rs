use crate::ring_buffer::Producer;
use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering, compiler_fence},
};
use critical_section::RestoreState;
use defmt::Encoder;

#[cfg(feature = "rtt")]
mod rtt;

#[cfg(feature = "qemu-test")]
mod semihosting;

#[cfg(feature = "async-await")]
pub(crate) static WAKER: crate::atomic_waker::AtomicWaker = crate::atomic_waker::AtomicWaker::new();

#[defmt::global_logger]
struct Logger;

pub(crate) struct LoggerState {
    producer: UnsafeCell<MaybeUninit<Producer<'static>>>,
    cs_state: UnsafeCell<RestoreState>,
    encoder: UnsafeCell<Encoder>,
    initialized: AtomicBool,
    /// Reentrancy depth counter. 0 = not logging, 1 = logging (owner), 2+ = reentrant.
    /// Reentrant calls (from NMI, HardFault, or panic during logging) are silently dropped.
    depth: AtomicUsize,
}

impl LoggerState {
    /// # Safety
    ///
    /// Must only be called once per program execution.
    pub(crate) unsafe fn initialize(&self, p: Producer<'static>) {
        // SAFETY: The caller guarantees this is called only once, so there is no data race
        // on the `producer` field. The `UnsafeCell` provides interior mutability.
        unsafe { self.producer.get().write(MaybeUninit::new(p)) };
        // Release: ensures the write to `producer` is visible before `initialized` becomes true.
        self.initialized.store(true, Ordering::Release);
    }

    /// # Safety
    ///
    /// Must be called from within a critical section to prevent aliasing of `producer`.
    #[inline]
    unsafe fn write(&self, bytes: &[u8]) {
        // Acquire: synchronizes with the Release store in `initialize`, ensuring we see
        // the fully initialized `producer`.
        if self.initialized.load(Ordering::Acquire) {
            // SAFETY: The Acquire load ensures `producer` is initialized. The critical section
            // (upheld by caller) ensures exclusive access, so creating `&mut` is safe.
            unsafe { &mut *self.producer.get().cast::<Producer>() }.write(bytes);
        }
    }
}

// SAFETY: All mutable access to fields is protected by either:
// - `initialized` flag with Acquire/Release ordering (for `producer`).
// - Critical sections (for `cs_state`, `encoder`, and `producer` during writes).
// The `initialized` flag uses atomic operations for thread-safe access.
unsafe impl Sync for LoggerState {}

pub(crate) static LOGGER_STATE: LoggerState = LoggerState {
    producer: UnsafeCell::new(MaybeUninit::uninit()),
    cs_state: UnsafeCell::new(RestoreState::invalid()),
    encoder: UnsafeCell::new(Encoder::new()),
    initialized: AtomicBool::new(false),
    depth: AtomicUsize::new(0),
};

/// Writes data to all configured outputs (ring buffer, RTT, and semihosting).
///
/// # Safety
///
/// Must be called from within a critical section.
#[inline(always)]
unsafe fn write_all(data: &[u8]) {
    // SAFETY: Caller guarantees we're in a critical section.
    unsafe { LOGGER_STATE.write(data) };
    #[cfg(feature = "rtt")]
    // SAFETY: Caller guarantees we're in a critical section.
    unsafe {
        rtt::write(data)
    };
    #[cfg(feature = "qemu-test")]
    // SAFETY: Caller guarantees we're in a critical section.
    unsafe {
        semihosting::write(data)
    };
}

// SAFETY: This impl upholds the `defmt::Logger` safety contract:
// - `acquire` enters a critical section before any logging operations.
// - `release` exits the critical section after logging is complete.
// - All mutable state access is protected by the critical section.
// - Reentrant calls (from NMI, HardFault, or panic during logging) are detected and dropped.
unsafe impl defmt::Logger for Logger {
    fn acquire() {
        // Increment depth. If we weren't at 0, we're reentrant and skip all setup.
        // This can happen if an NMI or HardFault fires during logging, or if
        // a panic handler tries to log while we're already logging.
        let was_depth = LOGGER_STATE.depth.fetch_add(1, Ordering::Acquire);
        if was_depth > 0 {
            return;
        }

        // SAFETY: This is the start of a logging operation. The critical section
        // will be released in `release()`. It's safe to acquire here as defmt
        // guarantees balanced acquire/release calls.
        let restore = unsafe { critical_section::acquire() };

        // Compiler fence ensures the critical section is fully entered before
        // we access shared state.
        compiler_fence(Ordering::SeqCst);

        // SAFETY: We're in a critical section, so exclusive access to `cs_state` is guaranteed.
        unsafe { LOGGER_STATE.cs_state.get().write(restore) };

        compiler_fence(Ordering::SeqCst);

        // SAFETY: We're in a critical section, so exclusive access to `encoder` is guaranteed.
        // The callback to `write_all` is also within the critical section.
        unsafe { &mut *LOGGER_STATE.encoder.get() }.start_frame(|b| unsafe { write_all(b) });
    }

    unsafe fn flush() {
        // Skip if reentrant (depth != 1).
        if LOGGER_STATE.depth.load(Ordering::Relaxed) != 1 {
            return;
        }

        #[cfg(feature = "rtt")]
        // SAFETY: Caller guarantees we're between acquire() and release().
        unsafe {
            rtt::flush()
        };
    }

    unsafe fn release() {
        // Decrement depth. If we weren't at 1, we're reentrant and skip all cleanup.
        let was_depth = LOGGER_STATE.depth.fetch_sub(1, Ordering::Release);
        if was_depth != 1 {
            return;
        }

        // SAFETY: We're still in the critical section from `acquire()`.
        // Exclusive access to `encoder` is guaranteed.
        unsafe { &mut *LOGGER_STATE.encoder.get() }.end_frame(|b| unsafe { write_all(b) });

        compiler_fence(Ordering::SeqCst);

        // SAFETY: We read the restore state that was saved in `acquire()` and release
        // the critical section. The critical section guarantees exclusive access to `cs_state`.
        unsafe { critical_section::release(LOGGER_STATE.cs_state.get().read()) };

        compiler_fence(Ordering::SeqCst);

        #[cfg(feature = "async-await")]
        WAKER.wake();
    }

    unsafe fn write(bytes: &[u8]) {
        // Skip if reentrant (depth != 1). The reentrant log is silently dropped.
        if LOGGER_STATE.depth.load(Ordering::Relaxed) != 1 {
            return;
        }

        // SAFETY: Caller (defmt) guarantees this is called between acquire() and release(),
        // so we're within a critical section. The encoder encodes the bytes and calls
        // our callback with the encoded data.
        unsafe { &mut *LOGGER_STATE.encoder.get() }.write(bytes, |b| unsafe { write_all(b) });
    }
}
