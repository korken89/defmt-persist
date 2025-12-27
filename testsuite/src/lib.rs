#![no_std]

use cortex_m_semihosting::debug::{self, EXIT_SUCCESS};
use panic_semihosting as _;

pub use cortex_m_rt::entry;
pub use defmt_persist;

pub fn exit_success() -> ! {
    debug::exit(EXIT_SUCCESS);
    #[allow(clippy::empty_loop)]
    loop {}
}
