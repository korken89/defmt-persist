# defmt-persist

[![CI](https://github.com/korken89/defmt-persist/actions/workflows/ci.yml/badge.svg)](https://github.com/korken89/defmt-persist/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/defmt-persist.svg)](https://crates.io/crates/defmt-persist)
[![docs.rs](https://docs.rs/defmt-persist/badge.svg)](https://docs.rs/defmt-persist)
[![License](https://img.shields.io/crates/l/defmt-persist.svg)](LICENSE-MIT)

A persistent `defmt` logger that survives resets.

This crate provides a `defmt::global_logger` that stores logged messages in a ring buffer,
similar to `defmt-brtt`. Unlike `defmt-brtt`, the buffer state persists across resets, so panic
messages and logs leading up to them can be transmitted after the device restarts. Basically
the combination of `panic-persist` (see `Panic Handler` section) and `defmt-brtt`.

## Setup

Reserve a memory region in your linker script for the persist buffer. This region must be
outside any section to prevent program initialization from zeroing or modifying it on boot.

Define `__defmt_persist_start` and `__defmt_persist_end` symbols pointing to this region.

Example `memory.x` before modification:

```text
MEMORY
{
  FLASH : ORIGIN = 0x00000000, LENGTH = 512K
  RAM   : ORIGIN = 0x20000000, LENGTH = 64K
}
```

After modification to reserve a 1K region:

```text
MEMORY
{
  FLASH         : ORIGIN = 0x00000000, LENGTH = 512K
  RAM           : ORIGIN = 0x20000000, LENGTH = 63K
  DEFMT_PERSIST : ORIGIN = 0x2000FC00, LENGTH = 1K
}

__defmt_persist_start = ORIGIN(DEFMT_PERSIST);
__defmt_persist_end   = ORIGIN(DEFMT_PERSIST) + LENGTH(DEFMT_PERSIST);
```

Then call `init` early in your program:

```rust,ignore
let Ok(mut consumer) = defmt_persist::init() else {
    panic!("init failed");
};
```

Use the returned `Consumer` to read and transmit buffered logs:

```rust
# fn transmit(_: (&[u8], &[u8])) -> usize { 0 }
# fn example(mut consumer: defmt_persist::Consumer<'_>) {
// Drain any persisted logs from before the reset
while !consumer.is_empty() {
    let grant = consumer.read();
    let len = transmit(grant.bufs()); // It's OK to not empty the entire grant, data is not lost
    grant.release(len);
}
# }
```

## Panic Handler

To capture panic messages that survive resets, define a panic handler that logs via defmt
before resetting:

```rust,ignore
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    defmt::error!("{}", defmt::Display2Format(info));
    cortex_m::peripheral::SCB::sys_reset(); // Or hardfault if it should go via fault handlers.
}
```

After reset, the panic message will be available in the persist buffer and can be read using
the `Consumer` from `init()`. See [`panic_test.rs`](testsuite/examples/panic_test.rs) for a
complete example.

Alternatively, [`panic-probe`](https://crates.io/crates/panic-probe) can be used for
hardfault-on-panic behavior.

## Bootloader Considerations

If your system uses a bootloader, the bootloader's linker script must also reserve/don't touch
the same memory region. Otherwise, the bootloader may use this memory for its stack or heap,
corrupting the persisted logs before the application starts. The bootloader can also call
`init` to read and transmit persisted logs from the application before launching it.

## Features

- `rtt`: Also output logs via RTT (default: enabled)
- `async-await`: Enable async API for waiting on new data (default: enabled)
- `ecc-64bit`: Add padding for MCUs with 64-bit ECC RAM, e.g. STM32H7/H5 (default: enabled)

## Testing

Run the library unit tests:

```bash
cargo test --all-features
```

Run the full QEMU-based integration testsuite (requires `qemu-system-arm`):

```bash
cargo xtask test [example_name]
```

This runs tests for persistence across resets, buffer corruption recovery, async API, and ring
buffer wraparound.

To run a single example in QEMU during development:

```bash
cargo xtask qemu <example_name>
```

## License

Licensed under either of Apache License, Version 2.0 or MIT license at your option.
