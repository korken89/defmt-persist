//! @test-run: persist
//! @test-validate: expected
//!
//! Identifier persistence test that runs in two phases:
//!
//! Phase 1 (fresh start): Set identifier to a known value, dump persist region.
//! Phase 2 (with snapshot): Verify recovered_identifier matches phase 1, set new identifier.

#![no_std]
#![no_main]

use defmt_persist::Identifier;
use testsuite::{drain_to_uart, dump_persist_region, entry, exit_failure, exit_success};

const PHASE1_ID: Identifier = Identifier([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
const PHASE2_ID: Identifier = Identifier([
    17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32,
]);

#[entry]
fn main() -> ! {
    let recovered_identifier = defmt_persist::peek_identifier().unwrap();
    let new_id = if recovered_identifier.is_none() {
        PHASE1_ID
    } else {
        PHASE2_ID
    };

    let mut consumer = defmt_persist::init(new_id).unwrap();

    defmt::info!("recovered_identifier: {:?}", recovered_identifier);
    defmt::info!("current_identifier: {:?}", consumer.identifier());

    if recovered_identifier.is_none() {
        // Phase 1: Fresh start, dump persist region for phase 2.
        defmt::info!("phase 1: fresh start");
        dump_persist_region();
        drain_to_uart(&mut consumer);
        exit_success();
    } else if recovered_identifier == Some(PHASE1_ID) {
        // Phase 2: Identifier correctly persisted.
        defmt::info!("phase 2: identifier correctly persisted!");
        drain_to_uart(&mut consumer);
        exit_success();
    } else {
        defmt::error!("unexpected identifier: {:?}", recovered_identifier);
        exit_failure();
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    defmt::error!("{}", defmt::Display2Format(info));
    exit_failure();
}
