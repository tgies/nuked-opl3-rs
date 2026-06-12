# nuked-opl3

`nuked-opl3` is a small pure-Rust implementation of the
[Nuked-OPL3](https://github.com/nukeykt/Nuked-OPL3) Yamaha OPL3 emulator.

# Usage

Nuked-OPL3 is not a turn-key implementation of the OPL3 chip - functions such as the status register, timers and
interrupts are left as implementation details.

You can access the Nuked-compatible chip API via the `Opl3Chip` struct, if needed, but with the caveat that directly
writing registers to the chip core will prevent you from reading the OPL registers correctly.

If you intend to utilize `nuked-opl3` in an emulator, you will probably want to use the `Opl3Device` wrapper which provides
a full OPL3 implementation including the status registers and timers.

# Cargo Features

- `stereo-ext`: match Nuked-OPL3 built with `OPL_ENABLE_STEREOEXT=1`. This enables the stereo-extension register
  behavior (`0x105` bit 1 and `0xD0..0xD8` pan registers) and mirrors Nuked's compile-time default of disabling the
  channel-sample-delay quirk in that build.
- `c-reference-tests`: build the vendored Nuked-OPL3 C source during tests and compare the Rust core against it.

# Benchmarks

The C-reference test feature includes ignored release benchmarks that compare the Rust core against the vendored
Nuked-OPL3 C core using trace fixtures generated from the workspace `crystal_oscillator.vgm` and
`oply-fork/tests/Intermission Fuck Yeah.imf` files. The same benchmark filter also includes one-iteration full-file
benchmarks for those workspace files:

```text
cargo test --release --features c-reference-tests bench -- --ignored --nocapture
cargo test --release --features c-reference-tests,stereo-ext bench -- --ignored --nocapture
```

Set `OPL3_RS_BENCH_ITERS` to override the default iteration count. Set `OPL3_RS_CRYSTAL_VGM` or
`OPL3_RS_INTERMISSION_IMF` to point the full-file benchmarks at different input files.

For a pure-C whole-file timing with no per-sample Rust FFI calls, build and run `tools/c_bench_full_file.c`.
On Windows, `tools\build_c_bench.ps1` uses the Visual Studio C++ tools and emits
`target\release\c_bench_full_file.exe`. Pass `-Compiler clang-cl` to build a `clang-cl` variant at
`target\release\c_bench_full_file_clang_cl.exe`, or pass `-NukedRoot` and `-OutputName` to build against
another Nuked-OPL3 source tree. On Unix-like hosts, use `tools/build_c_bench.sh`; it accepts
`--nuked-root` and `--output-name` for the same source-tree swap.

# Credits

[Nuked-OPL3](https://github.com/nukeykt/Nuked-OPL3) is (C) 2013-2020 Nuke.YKT and licensed under LGPL 2.1
