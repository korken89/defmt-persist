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
// The GNU build ID of the firmware, see "Firmware identifier" below.
let Ok(mut consumer) = defmt_persist::init(build_id()) else {
    panic!("init failed");
};
```

The argument to `init` is an identifier to store. The recovered logs can span several firmware
versions. Use `GrantR::identifier` to see which one wrote the data in a grant. To see which
firmware ran before, call `peek_identifier` before `init`.

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

## Firmware identifier

The identifier tells the host which ELF to decode the recovered logs with. The GNU build ID is a
good fit: the linker hashes the whole output ELF into it, including the `.defmt` string table
that decoding needs. So it changes whenever the logs need a different ELF to decode, also when
only a log string changed and the flash content stayed the same. It costs nothing at boot, and
the host reads the same ID from each ELF (`readelf -n`, or `build_id()` in the `object` crate).

Link with `--build-id=md5`, which gives exactly 16 bytes, e.g. in `build.rs`:

```rust,ignore
println!("cargo:rustc-link-arg=--build-id=md5");
```

Keep the note in the image and mark it in the linker script. Some runtimes discard all notes, a
section of its own comes first and keeps it:

```text
SECTIONS
{
  .note.gnu.build-id : ALIGN(4)
  {
    __build_id = .;
    KEEP(*(.note.gnu.build-id))
  } > FLASH
}
INSERT AFTER .rodata;
```

Then read it at boot:

```rust,ignore
/// An ELF note holding an MD5 GNU build ID.
#[repr(C)]
struct BuildIdNote {
    name_size: u32,
    desc_size: u32,
    kind: u32,
    name: [u8; 4],
    id: [u8; 16],
}

fn build_id() -> defmt_persist::Identifier {
    unsafe extern "C" {
        static __build_id: BuildIdNote;
    }
    // SAFETY: The linker script places the build ID note at `__build_id`.
    let note = unsafe { &*(&raw const __build_id) };
    // NT_GNU_BUILD_ID
    assert!(note.kind == 3 && note.name == *b"GNU\0" && note.desc_size == 16);
    defmt_persist::Identifier(note.id)
}
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

The bootloader and the application must use the same version of `defmt-persist` with the same
`ecc` setting. Otherwise they reinitialize each other's buffer on every boot.

## Features

- `rtt`: Also output logs via RTT (default: enabled)
- `async-await`: Enable async API for waiting on new data (default: enabled)
- `ecc`: Add ECC cache flush for MCUs with ECC-protected RAM (32-bit or 64-bit), e.g. STM32H7/H5 (default: enabled)

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
