# nuked-opl3

[![Crates.io](https://img.shields.io/crates/v/nuked-opl3.svg)](https://crates.io/crates/nuked-opl3)
[![Docs](https://img.shields.io/badge/docs-docs.rs-blue.svg)](https://docs.rs/nuked-opl3)
[![License](https://img.shields.io/crates/l/nuked-opl3.svg)](https://crates.io/crates/nuked-opl3)

A pure-Rust, sample-accurate emulation of the Yamaha YMF262 (OPL3), ported from
[Nuked-OPL3](https://github.com/nukeykt/Nuked-OPL3) by Nuke.YKT.

Produces bit-identical output to the C reference while incorporating performance
optimizations from [Nuked-OPL3-fast](https://github.com/tgies/Nuked-OPL3-fast),
making it *faster* than the original C implementation.

The Rust API was derived from [opl3-rs](https://github.com/dbalsom/opl3-rs) by
Daniel Balsom, an FFI wrapper around the original C implementation of
Nuked-OPL3. This repo was forked from opl3-rs; history prior to the v0.1.0 tag
is from opl3-rs.

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
nuked-opl3 = "0.1"
```

```rust
use nuked_opl3::Opl3Chip;

let mut chip = Opl3Chip::new(49716); // native OPL3 sample rate
chip.write_register(0x20, 0x01);     // set operator parameters
// ...configure more registers...

let mut buf = [0i16; 2]; // stereo output: [left, right]
chip.generate(&mut buf);
```

## `Opl3Device`

`Opl3Chip` is the raw synthesis core. It generates audio samples from register
writes, and nothing else. The real YMF262, however, also has a status register,
two timers, and an interrupt line. Nuked-OPL3 doesn't emulate any of these.

`Opl3Device` wraps `Opl3Chip` and adds:

- Status register (readable via `read_status()`)
- Timer 1 & Timer 2 with masking and overflow flags
- Register tracking (read back the last value written to any register)

Use `Opl3Device` when you need timer-driven playback or status register reads
(e.g. emulating a Sound Blaster or AdLib). Use `Opl3Chip` directly when you
only need the synthesis engine.

## Cargo Features

| Feature | Description |
|---------|-------------|
| `stereo-ext` | Enables stereo extension pan registers (equivalent to `OPL_ENABLE_STEREOEXT=1` in upstream) |
| `c-reference-tests` | Builds the vendored C Nuked-OPL3 source for bit-exact parity tests and benchmarks |

## Examples

The `examples/` directory contains two example programs (not published with the
crate):

- `play_tune` — synthesizes a short tune and plays it through the speakers.
- `play_file` — plays back IMF and VGM files with live audio output or WAV
  rendering.

## Benchmarks

```bash
cargo bench
```

Enable the `c-reference-tests` feature to include Rust-vs-C comparison
benchmarks:

```bash
cargo bench --features c-reference-tests
```

## Credits

- Nuke.YKT — original [Nuked-OPL3](https://github.com/nukeykt/Nuked-OPL3) C implementation
- Daniel Balsom — original Rust FFI bindings that this project was forked from

## License

LGPL-2.1-or-later. See [LICENSE](LICENSE) for details.
