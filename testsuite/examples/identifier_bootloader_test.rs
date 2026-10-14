//! @test-run: persist
//! @test-validate: expected
//!
//! A bootloader and an application that both log, without the logs being read in between:
//!
//! Phase 1: The application logs and resets.
//! Phase 2: The bootloader logs and resets.
//! Phase 3: The application again, the logs of both must be kept and identified.

#![no_std]
#![no_main]

use defmt_persist::Identifier;
use testsuite::{drain_with_identifiers, dump_persist_region, entry, exit_failure, exit_success};

const APPLICATION: Identifier = Identifier([0xa; 16]);
const BOOTLOADER: Identifier = Identifier([0xb; 16]);

#[entry]
fn main() -> ! {
    let recovered_identifier = defmt_persist::peek_identifier().unwrap();
    let (phase, identifier) = match recovered_identifier {
        None => (1, APPLICATION),
        Some(APPLICATION) => (2, BOOTLOADER),
        Some(_) => (3, APPLICATION),
    };

    let mut consumer = defmt_persist::init(identifier.clone()).unwrap();
    defmt::info!("phase {}: recovered {:?}", phase, recovered_identifier);

    if phase < 3 {
        // Reset without reading the logs.
        defmt::info!("phase {}: logged by {:?}", phase, identifier);
        dump_persist_region();
    } else {
        drain_with_identifiers(&mut consumer);
    }
    exit_success();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    defmt::error!("{}", defmt::Display2Format(info));
    exit_failure();
}
