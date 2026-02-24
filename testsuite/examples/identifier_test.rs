//! @test-run: persist
//! @test-validate: expected
//!
//! Identifier persistence test that runs in two phases:
//!
//! Phase 1 (fresh start): Set identifier to a known value, dump persist region.
//! Phase 2 (with snapshot): Verify recovered_identifier matches phase 1, set new identifier.

#![no_std]
#![no_main]

use defmt_persist::{ConsumerAndMetadata, Identifier};
use testsuite::{drain_to_uart, dump_persist_region, entry, exit_failure, exit_success};

const PHASE1_ID: Identifier = Identifier([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
const PHASE2_ID: Identifier = Identifier([
    17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 26, 27, 28, 29, 30, 31,
]);
const ZEROED_ID: Identifier = Identifier([0; 16]);

#[entry]
fn main() -> ! {
    let metadata = defmt_persist::init(|old| {
        if old == &ZEROED_ID {
            PHASE1_ID
        } else {
            PHASE2_ID
        }
    })
    .unwrap();
    let ConsumerAndMetadata {
        mut consumer,
        recovered_logs_len: _,
        recovered_identifier,
    } = metadata;

    defmt::info!("recovered_identifier: {:?}", recovered_identifier);
    defmt::info!("current_identifier: {:?}", consumer.identifier());

    if recovered_identifier == ZEROED_ID {
        // Phase 1: Fresh start, dump persist region for phase 2.
        defmt::info!("phase 1: fresh start");
        dump_persist_region();
        drain_to_uart(&mut consumer);
        exit_success();
    } else if recovered_identifier == PHASE1_ID {
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
