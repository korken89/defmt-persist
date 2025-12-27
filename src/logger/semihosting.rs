//! Semihosting output for QEMU testing.
//!
//! When the `qemu-test` feature is enabled, defmt frames are written to
//! semihosting stdout, allowing the test harness to capture and decode them.

use core::cell::UnsafeCell;
use cortex_m_semihosting::hio::{self, HostStream};

static STDOUT: SyncCell<Option<HostStream>> = SyncCell(UnsafeCell::new(None));

struct SyncCell<T>(UnsafeCell<T>);

// SAFETY: Access is protected by critical sections (ensured by caller).
unsafe impl<T> Sync for SyncCell<T> {}

/// Writes bytes to semihosting stdout.
///
/// # Safety
///
/// Must be called from within a critical section to prevent concurrent access.
pub(crate) unsafe fn write(bytes: &[u8]) {
    // SAFETY: Caller guarantees we're in a critical section.
    let handle = unsafe { &mut *STDOUT.0.get() };

    // Lazily initialize stdout handle (only once, to avoid W_TRUNC on reopens).
    if handle.is_none() {
        *handle = hio::hstdout().ok();
    }

    if let Some(stdout) = handle {
        let _ = stdout.write_all(bytes);
    }
}
