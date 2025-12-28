# defmt-persist

[![CI](https://github.com/korken89/defmt-persist/actions/workflows/ci.yml/badge.svg)](https://github.com/korken89/defmt-persist/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/defmt-persist.svg)](https://crates.io/crates/defmt-persist)
[![docs.rs](https://docs.rs/defmt-persist/badge.svg)](https://docs.rs/defmt-persist)
[![License](https://img.shields.io/crates/l/defmt-persist.svg)](LICENSE-MIT)

A persistent `defmt` logger that survives resets.

This crate provides a `defmt::global_logger` that stores logged messages in a
ring buffer, similar to `defmt-brtt`. Unlike `defmt-brtt`, the buffer state persists
across resets, so panic messages and logs leading up to them can be transmitted
after the device restarts. Basically the combination of `panic-persist` and `defmt-brtt`.

## Setup

Reserve a memory region in your linker script for the persist buffer. This region must
be outside any section to prevent program initialization from zeroing or modifying it
on boot.

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

**Important:** If your system uses a bootloader, the bootloader's linker script must
also reserve the same memory region. Otherwise, the bootloader may use this memory
for its stack or heap, corrupting the persisted logs before the application starts.
The bootloader can also call `init` to read and transmit persisted logs from the
application before launching it.

Then call `init` early in your program and use the returned `Consumer` to
read and transmit buffered logs:

```rust,ignore
let Some(mut consumer) = defmt_persist::init() else {
    panic!("Already initialized");
};

// Drain any persisted logs from before the reset
loop {
    let grant = consumer.read();
    if grant.buf().is_empty() {
        break;
    }
    transmit(grant.buf());
    let len = grant.buf().len();
    grant.release(len);
}
```

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

This runs tests for persistence across resets, buffer corruption recovery, async API, and ring buffer wraparound.

To run a single example in QEMU during development:

```bash
cargo xtask qemu <example_name>
```

## License

Licensed under either of Apache License, Version 2.0 or MIT license at your option.
