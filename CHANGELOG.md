# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [0.1.0] - 2026-06-12

Initial release. Pure Rust port derived from [Nuked-OPL3](https://github.com/nukeykt/Nuked-OPL3) v1.8.

### Added

- Pure Rust port of the Nuked-OPL3 1.8 chip core — no C code or FFI required at
  runtime.
- Performance optimizations (silent-regime shortcut, pre-shifted EXPROM, cached EG
  rates, flat output-array indices) that make the Rust core faster than the upstream
  C implementation while remaining sample-accurate.
- `Opl3Device` wrapper providing a full device-level OPL3 implementation with status
  register, address/data registers, and timers.
- `stereo-ext` cargo feature for Nuked-OPL3's stereo extension pan registers.
- `c-reference-tests` cargo feature for sample-accurate parity testing against the
  vendored C source.
- VGM, IMF, and DRO playback examples.

[0.1.0]: https://github.com/tgies/nuked-opl3-rs/releases/tag/v0.1.0
