//! @test-run: persist
//! @test-validate: expected
//!
//! Firmware change followed by another reset before the logs are read:
//!
//! Phase 1: Firmware A logs and resets.
//! Phase 2: Firmware B logs and resets.
//! Phase 3: Firmware B again, the logs from phase 1 must still be from A.

#![no_std]
#![no_main]

use defmt_persist::{ConsumerAndMetadata, Identifier};
use testsuite::{drain_with_identifiers, dump_persist_region, entry, exit_failure, exit_success};

const A: Identifier = Identifier([0xa; 16]);
const B: Identifier = Identifier([0xb; 16]);

#[entry]
fn main() -> ! {
    let (phase, identifier) = match defmt_persist::peek_identifier().unwrap() {
        None => (1, A),
        Some(A) => (2, B),
        Some(_) => (3, B),
    };

    let ConsumerAndMetadata {
        mut consumer,
        recovered_logs_len: _,
        recovered_identifier,
    } = defmt_persist::init(identifier.clone()).unwrap();
    defmt::info!("phase {}: recovered {:?}", phase, recovered_identifier);

    if phase < 3 {
        // Reset without reading the logs.
        defmt::info!("phase {}: logged by {:?}", phase, identifier);
        dump_persist_region();
    } else {
        // The logs from phase 1 are from A, the rest from B.
        drain_with_identifiers(&mut consumer);
    }
    exit_success();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    defmt::error!("{}", defmt::Display2Format(info));
    exit_failure();
}
