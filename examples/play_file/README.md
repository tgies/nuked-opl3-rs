# play_file

Live IMF/VGM playback example for `nuked-opl3`.

```bash
cargo run -p play_file -- ../crystal_oscillator.vgm
cargo run -p play_file -- "../oply-fork/tests/Intermission Fuck Yeah.imf"
```

Useful options:

```bash
--backend NAME    Select auto, pulse, or rodio. Defaults to auto.
--buffered        Queue OPL writes through the chip write buffer.
--chunk-frames N  Audio frames per live write. Defaults to 4096.
--device NAME     Rodio: output device name substring. Pulse: sink name.
--dry-run         Parse the file and print metadata without opening audio.
--imf-rate HZ     Set the IMF tick rate. Defaults to 700.
--list-devices    List output devices and exit.
--prebuffer-ms MS Target live play-ahead headroom. Defaults to 500.
--sample-rate HZ  Set WAV render rate. Defaults to 44100.
--status          Show live playback time, active voices, RT ratio, and buffer.
--sync-writes     Apply writes immediately. Also available as --unbuffered.
--tail-ms MS      Generate an extra release tail after the file ends.
--wav FILE        Render to a WAV file instead of opening live audio.
```

On Linux, `auto` prefers PulseAudio when `PULSE_SERVER` or WSLg's Pulse socket
is present, matching the path used by tools like `ffplay`. The `rodio` backend
uses CPAL, which may only see ALSA/JACK devices; `--list-devices` reports that
backend's output devices.

The VGM parser supports YM3812 writes (`0x5a`), YMF262 writes (`0x5e`/`0x5f`),
wait commands, end markers, data blocks, and skips known non-OPL chip command
payloads. VGM playback uses buffered OPL writes by default to model YMF262 write
latency; use `--sync-writes` only when debugging immediate-write behavior. IMF
playback remains synchronous by default, with `--buffered` available as an
explicit override. It does not currently decompress `.vgz` input.
