#![no_std]

pub mod uart;

use cortex_m_semihosting::debug::{self, EXIT_SUCCESS};
use defmt_persist as _;
use panic_semihosting as _;

pub use cortex_m_rt::entry;

pub fn exit_success() -> ! {
    debug::exit(EXIT_SUCCESS);
    #[allow(clippy::empty_loop)]
    loop {}
}

/// Dump the PERSIST region via UART1.
///
/// This outputs the raw bytes of the persist region via UART1,
/// which can be captured and used to initialize the region in
/// a subsequent test.
pub fn dump_persist_region() {
    unsafe extern "C" {
        static __defmt_persist_start: u8;
        static __defmt_persist_end: u8;
    }

    let start = (&raw const __defmt_persist_start) as *const u8;
    let end = (&raw const __defmt_persist_end) as *const u8;
    let len = end as usize - start as usize;
    let persist_data = unsafe { core::slice::from_raw_parts(start, len) };
    uart::write_bytes_uart1(persist_data);
}

/// Dump the PERSIST region via UART1 and exit successfully.
pub fn dump_persist_region_and_exit() -> ! {
    dump_persist_region();
    exit_success();
}
