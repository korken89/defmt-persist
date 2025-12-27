#![no_std]

pub mod uart;

use cortex_m_semihosting::debug::{self, EXIT_SUCCESS};
use defmt_persist as _;
use panic_semihosting as _;

pub use cortex_m_rt::entry;

// Provide a dummy timestamp for defmt (no real timer in QEMU tests)
defmt::timestamp!("{=u64}", 0);

pub fn exit_success() -> ! {
    debug::exit(EXIT_SUCCESS);
    #[allow(clippy::empty_loop)]
    loop {}
}
