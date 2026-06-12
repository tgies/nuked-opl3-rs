use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};

use std::num::NonZero;

use nuked_opl3::Opl3Chip;
use rodio::buffer::SamplesBuffer;
use rodio::cpal;
use rodio::cpal::traits::{DeviceTrait, HostTrait};
use rodio::Source;

const VGM_RATE: u32 = 44_100;
const DRO_RATE: u32 = 1_000;
const DEFAULT_IMF_RATE: u32 = 700;
const DEFAULT_WAV_RATE: u32 = 44_100;
const DEFAULT_CHUNK_FRAMES: usize = 4096;
const DEFAULT_PREBUFFER_MS: u64 = 500;
const RENDER_QUEUE_CHUNKS: usize = 2;
const STATUS_INTERVAL: Duration = Duration::from_millis(250);

type Result<T> = std::result::Result<T, PlayerError>;

#[derive(Debug)]
struct PlayerError(String);

impl PlayerError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for PlayerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for PlayerError {}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Event {
    Write { reg: u16, value: u8 },
    Wait(u64),
    End,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Format {
    Imf,
    Vgm,
    Dro,
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Imf => f.write_str("IMF"),
            Self::Vgm => f.write_str("VGM"),
            Self::Dro => f.write_str("DRO"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Backend {
    Auto,
    Pulse,
    Rodio,
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Pulse => f.write_str("pulse"),
            Self::Rodio => f.write_str("rodio"),
        }
    }
}

#[derive(Clone, Debug)]
struct Song {
    format: Format,
    events: Vec<Event>,
    timebase_rate: u32,
    writes: usize,
    wait_ticks: u64,
}

#[derive(Debug)]
struct Options {
    path: Option<PathBuf>,
    chunk_frames: usize,
    imf_rate: u32,
    backend: Backend,
    buffered: bool,
    device: Option<String>,
    dry_run: bool,
    list_devices: bool,
    prebuffer_ms: u64,
    sample_rate: u32,
    status: bool,
    sync_writes: bool,
    tail_ms: u64,
    wav_output: Option<PathBuf>,
}

fn main() -> std::result::Result<(), Box<dyn Error>> {
    let mut opts = parse_args(env::args_os().skip(1))?;

    if opts.list_devices {
        list_output_devices()?;
        return Ok(());
    }

    let path = opts
        .path
        .as_ref()
        .ok_or_else(|| PlayerError::new(usage()))?;
    let data = fs::read(path).map_err(|err| {
        PlayerError::new(format!("failed to read {}: {err}", path.to_string_lossy()))
    })?;
    let song = parse_song(path, &data, opts.imf_rate)?;
    opts.buffered = effective_buffered(&opts, song.format);
    let duration = song.wait_ticks as f64 / song.timebase_rate as f64;

    if opts.dry_run {
        println!(
            "parsed {} as {}: {} writes, {:.2}s, timebase {} Hz, buffered writes: {}",
            path.display(),
            song.format,
            song.writes,
            duration,
            song.timebase_rate,
            opts.buffered
        );
        return Ok(());
    }

    if let Some(wav_output) = opts.wav_output.as_ref() {
        println!(
            "rendering {} as {}: {} writes, {:.2}s, {} Hz WAV, buffered writes: {}",
            path.display(),
            song.format,
            song.writes,
            duration,
            opts.sample_rate,
            opts.buffered
        );
        render_wav(
            wav_output,
            OplFileSource::new(
                song.events,
                song.timebase_rate,
                opts.sample_rate,
                opts.buffered,
                opts.tail_ms,
            ),
        )?;
        println!("wrote {}", wav_output.display());
        return Ok(());
    }

    play_live(path, song, duration, &opts)?;
    Ok(())
}

fn usage() -> &'static str {
    "usage: play_file [--backend auto|pulse|rodio] [--buffered|--sync-writes] [--chunk-frames N] [--device NAME] [--dry-run] [--imf-rate HZ] [--list-devices] [--prebuffer-ms MS] [--sample-rate HZ] [--status] [--tail-ms MS] [--wav FILE] FILE.imf|FILE.vgm|FILE.dro"
}

fn list_output_devices() -> Result<()> {
    let host = cpal::default_host();
    let devices = host
        .output_devices()
        .map_err(|err| PlayerError::new(format!("failed to enumerate output devices: {err}")))?;

    let mut count = 0usize;
    for (idx, device) in devices.enumerate() {
        count += 1;
        #[allow(deprecated)]
        let name = device.name().unwrap_or_else(|_| String::from("<unknown>"));
        println!("{idx}: {name}");
    }

    if count == 0 {
        println!("no output devices reported by {}", host.id().name());
    }
    Ok(())
}

fn play_live(path: &Path, song: Song, duration: f64, opts: &Options) -> Result<()> {
    match opts.backend {
        Backend::Pulse => play_pulse(path, song, duration, opts),
        Backend::Rodio => play_rodio(path, song, duration, opts),
        Backend::Auto => {
            if should_prefer_pulse() {
                match play_pulse(path, song.clone(), duration, opts) {
                    Ok(()) => Ok(()),
                    Err(pulse_err) => {
                        eprintln!("PulseAudio backend failed: {pulse_err}");
                        eprintln!("falling back to Rodio/CPAL");
                        play_rodio(path, song, duration, opts)
                    }
                }
            } else {
                match play_rodio(path, song.clone(), duration, opts) {
                    Ok(()) => Ok(()),
                    Err(rodio_err) => {
                        eprintln!("Rodio/CPAL backend failed: {rodio_err}");
                        eprintln!("falling back to PulseAudio");
                        play_pulse(path, song, duration, opts)
                    }
                }
            }
        }
    }
}

fn should_prefer_pulse() -> bool {
    cfg!(target_os = "linux")
        && (env::var_os("PULSE_SERVER").is_some() || Path::new("/mnt/wslg/PulseServer").exists())
}

fn effective_buffered(opts: &Options, format: Format) -> bool {
    opts.buffered || (format == Format::Vgm && !opts.sync_writes)
}

fn play_rodio(path: &Path, song: Song, duration: f64, opts: &Options) -> Result<()> {
    let audio_device = choose_output_device(opts.device.as_deref())?;
    #[allow(deprecated)]
    let device_name = audio_device
        .name()
        .unwrap_or_else(|_| String::from("<unknown>"));
    let config = audio_device.default_output_config().map_err(|err| {
        PlayerError::new(format!(
            "failed to read audio output config for {device_name}: {err}\n\
             In WSL, this usually means no ALSA output is available. Try `--backend pulse`, \
             configure WSL audio, or render with `--wav out.wav`."
        ))
    })?;
    let sample_rate = config.sample_rate();

    let stream = rodio::DeviceSinkBuilder::from_device(audio_device)
        .map_err(|err| PlayerError::new(format!("failed to open audio output stream: {err}")))?
        .open_stream()
        .map_err(|err| PlayerError::new(format!("failed to open audio output stream: {err}")))?;
    let sink = rodio::Player::connect_new(&stream.mixer());
    let playahead_target = playahead_target_frames(opts, sample_rate);

    println!(
        "playing {} as {}: {} writes, {:.2}s, {} Hz output, backend: rodio, buffered writes: {}, play-ahead target: {} ms",
        path.display(),
        song.format,
        song.writes,
        duration,
        sample_rate,
        opts.buffered,
        frames_to_ms(playahead_target, sample_rate)
    );
    println!("audio device: {device_name}");

    let live_status = Arc::new(LiveStatus::default());
    let mut status = StatusPrinter::new(opts.status, sample_rate);
    let source = OplFileSource::new(
        song.events,
        song.timebase_rate,
        sample_rate,
        opts.buffered,
        opts.tail_ms,
    )
    .with_status(Some(live_status.clone()), opts.status);
    let (render_rx, render_thread) = spawn_render_thread(source, opts.chunk_frames);

    let mut playback_start = None;
    let mut frames_queued = 0u64;
    let mut source_finished = false;

    while !source_finished || !sink.empty() {
        while !source_finished
            && should_queue_more(playback_start, frames_queued, sample_rate, playahead_target)
        {
            match render_rx.recv() {
                Ok(Some(chunk)) => {
                    let chunk_frames = stereo_frame_count(&chunk);
                    let was_not_started = playback_start.is_none();
                    let start = *playback_start.get_or_insert_with(Instant::now);
                    frames_queued = frames_queued.saturating_add(chunk_frames);
                    live_status.record_queued_frames(frames_queued);
                    if opts.status && was_not_started {
                        status.start_playback(&live_status, start);
                    }
                    sink.append(SamplesBuffer::new(
                        NonZero::new(2).unwrap(),
                        NonZero::new(sample_rate).unwrap(),
                        chunk,
                    ));
                    status.playback(&live_status);
                }
                Ok(None) | Err(_) => {
                    source_finished = true;
                }
            }
        }

        if source_finished && sink.empty() {
            break;
        }

        status.playback(&live_status);
        std::thread::sleep(Duration::from_millis(if opts.status { 50 } else { 25 }));
    }

    status.finish();
    render_thread
        .join()
        .map_err(|_| PlayerError::new("render thread panicked"))?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn play_pulse(path: &Path, song: Song, duration: f64, opts: &Options) -> Result<()> {
    let sample_rate = opts.sample_rate;
    let playahead_target = playahead_target_frames(opts, sample_rate);
    println!(
        "playing {} as {}: {} writes, {:.2}s, {} Hz output, backend: pulse, buffered writes: {}, play-ahead target: {} ms",
        path.display(),
        song.format,
        song.writes,
        duration,
        sample_rate,
        opts.buffered,
        frames_to_ms(playahead_target, sample_rate)
    );

    let stream_name = format!("{} {}", song.format, path.display());
    let live_status = Arc::new(LiveStatus::default());
    let mut status = StatusPrinter::new(opts.status, sample_rate);
    let source = OplFileSource::new(
        song.events,
        song.timebase_rate,
        sample_rate,
        opts.buffered,
        opts.tail_ms,
    )
    .with_status(Some(live_status.clone()), opts.status);
    let (render_rx, render_thread) = spawn_render_thread(source, opts.chunk_frames);

    let mut stream = pulse::PulseStream::new(sample_rate, opts.device.as_deref(), &stream_name)?;
    let mut playback_start = None;
    let mut frames_queued = 0u64;
    let mut source_finished = false;
    let mut write_result = Ok(());

    while !source_finished {
        while !source_finished
            && should_queue_more(playback_start, frames_queued, sample_rate, playahead_target)
        {
            match render_rx.recv() {
                Ok(Some(chunk)) => {
                    let chunk_frames = stereo_frame_count(&chunk);
                    if let Err(err) = stream.write_f32(&chunk) {
                        write_result = Err(err);
                        source_finished = true;
                        break;
                    }
                    let was_not_started = playback_start.is_none();
                    let start = *playback_start.get_or_insert_with(Instant::now);
                    frames_queued = frames_queued.saturating_add(chunk_frames);
                    live_status.record_queued_frames(frames_queued);
                    if opts.status && was_not_started {
                        status.start_playback(&live_status, start);
                    }
                    status.playback(&live_status);
                }
                Ok(None) | Err(_) => {
                    source_finished = true;
                }
            }
        }

        if !source_finished {
            status.playback(&live_status);
            std::thread::sleep(Duration::from_millis(if opts.status { 50 } else { 25 }));
        }
    }
    drop(render_rx);
    render_thread
        .join()
        .map_err(|_| PlayerError::new("render thread panicked"))?;

    let result = match write_result {
        Ok(()) => stream.drain(),
        Err(err) => Err(err),
    };
    status.finish();
    result
}

#[cfg(not(target_os = "linux"))]
fn play_pulse(_path: &Path, _song: Song, _duration: f64, _opts: &Options) -> Result<()> {
    Err(PlayerError::new(
        "PulseAudio backend is only implemented for Linux",
    ))
}

fn choose_output_device(name_filter: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();

    if let Some(name_filter) = name_filter {
        let needle = name_filter.to_ascii_lowercase();
        let devices = host.output_devices().map_err(|err| {
            PlayerError::new(format!("failed to enumerate output devices: {err}"))
        })?;

        for device in devices {
            #[allow(deprecated)]
            let name = device.name().unwrap_or_else(|_| String::from("<unknown>"));
            if name.to_ascii_lowercase().contains(&needle) {
                return Ok(device);
            }
        }

        Err(PlayerError::new(format!(
            "no output device matched {name_filter:?}; run `--list-devices`"
        )))
    } else {
        host.default_output_device().ok_or_else(|| {
            PlayerError::new(
                "no default audio output device found. In WSL, configure an audio backend, \
                 run `--list-devices`, or render with `--wav out.wav`.",
            )
        })
    }
}

fn render_wav(path: &Path, source: OplFileSource) -> Result<()> {
    let sample_rate = source.sample_rate;
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|err| PlayerError::new(format!("failed to create {}: {err}", path.display())))?;

    for sample in source {
        writer
            .write_sample(sample)
            .map_err(|err| PlayerError::new(format!("failed to write WAV sample: {err}")))?;
    }

    writer
        .finalize()
        .map_err(|err| PlayerError::new(format!("failed to finalize {}: {err}", path.display())))
}

fn parse_args<I>(args: I) -> Result<Options>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    let mut path = None;
    let mut chunk_frames = DEFAULT_CHUNK_FRAMES;
    let mut imf_rate = DEFAULT_IMF_RATE;
    let mut backend = Backend::Auto;
    let mut buffered = false;
    let mut device = None;
    let mut dry_run = false;
    let mut list_devices = false;
    let mut prebuffer_ms = DEFAULT_PREBUFFER_MS;
    let mut sample_rate = DEFAULT_WAV_RATE;
    let mut status = false;
    let mut sync_writes = false;
    let mut tail_ms = 0;
    let mut wav_output = None;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        let Some(text) = arg.to_str() else {
            return Err(PlayerError::new("arguments must be valid UTF-8"));
        };

        if text == "-h" || text == "--help" {
            println!("{}", usage());
            std::process::exit(0);
        } else if text == "--backend" {
            backend = parse_backend(&parse_string_arg("--backend", args.next())?)?;
        } else if let Some(value) = text.strip_prefix("--backend=") {
            backend = parse_backend(value)?;
        } else if text == "-b" || text == "--buffered" {
            buffered = true;
        } else if text == "--chunk" || text == "--chunk-frames" {
            chunk_frames = parse_usize_arg(text, args.next())?;
        } else if let Some(value) = text.strip_prefix("--chunk=") {
            chunk_frames = parse_usize("--chunk", value)?;
        } else if let Some(value) = text.strip_prefix("--chunk-frames=") {
            chunk_frames = parse_usize("--chunk-frames", value)?;
        } else if text == "--device" {
            device = Some(parse_string_arg("--device", args.next())?);
        } else if let Some(value) = text.strip_prefix("--device=") {
            device = Some(value.to_string());
        } else if text == "--dry-run" {
            dry_run = true;
        } else if text == "--imf-rate" {
            imf_rate = parse_u32_arg("--imf-rate", args.next())?;
        } else if let Some(value) = text.strip_prefix("--imf-rate=") {
            imf_rate = parse_rate("--imf-rate", value)?;
        } else if text == "--list-devices" {
            list_devices = true;
        } else if text == "--prebuffer-ms" {
            prebuffer_ms = parse_u64_arg("--prebuffer-ms", args.next())?;
        } else if let Some(value) = text.strip_prefix("--prebuffer-ms=") {
            prebuffer_ms = parse_duration_arg("--prebuffer-ms", value)?;
        } else if text == "--sample-rate" {
            sample_rate = parse_u32_arg("--sample-rate", args.next())?;
        } else if let Some(value) = text.strip_prefix("--sample-rate=") {
            sample_rate = parse_rate("--sample-rate", value)?;
        } else if text == "-s" || text == "--status" {
            status = true;
        } else if text == "--sync-writes" || text == "--unbuffered" {
            sync_writes = true;
        } else if text == "--tail-ms" {
            tail_ms = parse_u64_arg("--tail-ms", args.next())?;
        } else if let Some(value) = text.strip_prefix("--tail-ms=") {
            tail_ms = parse_duration_arg("--tail-ms", value)?;
        } else if text == "--wav" {
            wav_output = Some(parse_path_arg("--wav", args.next())?);
        } else if let Some(value) = text.strip_prefix("--wav=") {
            wav_output = Some(PathBuf::from(value));
        } else if text.starts_with('-') {
            return Err(PlayerError::new(format!(
                "unknown option {text}\n{}",
                usage()
            )));
        } else if path.is_none() {
            path = Some(PathBuf::from(text));
        } else {
            return Err(PlayerError::new(format!(
                "unexpected extra argument {text}\n{}",
                usage()
            )));
        }
    }

    if !list_devices && path.is_none() {
        return Err(PlayerError::new(usage()));
    }
    if imf_rate == 0 {
        return Err(PlayerError::new("--imf-rate must be greater than zero"));
    }
    if sample_rate == 0 {
        return Err(PlayerError::new("--sample-rate must be greater than zero"));
    }
    if chunk_frames == 0 {
        return Err(PlayerError::new("--chunk-frames must be greater than zero"));
    }
    if buffered && sync_writes {
        return Err(PlayerError::new(
            "--buffered and --sync-writes cannot be used together",
        ));
    }

    Ok(Options {
        path,
        chunk_frames,
        imf_rate,
        backend,
        buffered,
        device,
        dry_run,
        list_devices,
        prebuffer_ms,
        sample_rate,
        status,
        sync_writes,
        tail_ms,
        wav_output,
    })
}

fn parse_string_arg(name: &str, value: Option<std::ffi::OsString>) -> Result<String> {
    let value = value.ok_or_else(|| PlayerError::new(format!("{name} requires a value")))?;
    value
        .into_string()
        .map_err(|_| PlayerError::new(format!("{name} must be valid UTF-8")))
}

fn parse_path_arg(name: &str, value: Option<std::ffi::OsString>) -> Result<PathBuf> {
    let value = value.ok_or_else(|| PlayerError::new(format!("{name} requires a value")))?;
    Ok(PathBuf::from(value))
}

fn parse_backend(value: &str) -> Result<Backend> {
    match value {
        "auto" => Ok(Backend::Auto),
        "pulse" | "pulseaudio" => Ok(Backend::Pulse),
        "rodio" | "cpal" | "alsa" => Ok(Backend::Rodio),
        _ => Err(PlayerError::new(format!(
            "--backend must be auto, pulse, or rodio, got {value:?}"
        ))),
    }
}

fn parse_u32_arg(name: &str, value: Option<std::ffi::OsString>) -> Result<u32> {
    let value = value.ok_or_else(|| PlayerError::new(format!("{name} requires a value")))?;
    let value = value
        .to_str()
        .ok_or_else(|| PlayerError::new(format!("{name} must be valid UTF-8")))?;
    parse_rate(name, value)
}

fn parse_u64_arg(name: &str, value: Option<std::ffi::OsString>) -> Result<u64> {
    let value = value.ok_or_else(|| PlayerError::new(format!("{name} requires a value")))?;
    let value = value
        .to_str()
        .ok_or_else(|| PlayerError::new(format!("{name} must be valid UTF-8")))?;
    parse_duration_arg(name, value)
}

fn parse_usize_arg(name: &str, value: Option<std::ffi::OsString>) -> Result<usize> {
    let value = value.ok_or_else(|| PlayerError::new(format!("{name} requires a value")))?;
    let value = value
        .to_str()
        .ok_or_else(|| PlayerError::new(format!("{name} must be valid UTF-8")))?;
    parse_usize(name, value)
}

fn parse_rate(name: &str, value: &str) -> Result<u32> {
    value
        .parse::<u32>()
        .map_err(|_| PlayerError::new(format!("{name} must be an integer")))
}

fn parse_usize(name: &str, value: &str) -> Result<usize> {
    value
        .parse::<usize>()
        .map_err(|_| PlayerError::new(format!("{name} must be an integer")))
}

fn parse_duration_arg(name: &str, value: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .map_err(|_| PlayerError::new(format!("{name} must be an integer")))
}

fn parse_song(path: &Path, data: &[u8], imf_rate: u32) -> Result<Song> {
    let format = detect_format(path, data)?;
    match format {
        Format::Imf => parse_imf(data, imf_rate),
        Format::Vgm => parse_vgm(data),
        Format::Dro => parse_dro(data),
    }
}

fn detect_format(path: &Path, data: &[u8]) -> Result<Format> {
    if data.starts_with(b"Vgm ") {
        return Ok(Format::Vgm);
    }
    if data.starts_with(b"DBRAWOPL") {
        return Ok(Format::Dro);
    }
    if data.starts_with(&[0x1f, 0x8b]) {
        return Err(PlayerError::new(
            "VGZ/gzip input is not supported yet; decompress to .vgm first",
        ));
    }

    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("imf") => Ok(Format::Imf),
        Some("vgm") => Ok(Format::Vgm),
        Some("dro") => Ok(Format::Dro),
        Some(other) => Err(PlayerError::new(format!(
            "unknown file extension .{other}; expected .imf, .vgm, or .dro"
        ))),
        None => Err(PlayerError::new(
            "could not infer file format; expected .imf, .vgm, or .dro",
        )),
    }
}

fn parse_imf(data: &[u8], imf_rate: u32) -> Result<Song> {
    let body = if data.len() >= 6 {
        let declared_len = read_u16_at(data, 0)? as usize;
        if declared_len == data.len() - 2 && declared_len % 4 == 0 {
            &data[2..]
        } else {
            data
        }
    } else {
        data
    };

    if body.len() % 4 != 0 {
        return Err(PlayerError::new(format!(
            "IMF body length {} is not a multiple of 4 bytes",
            body.len()
        )));
    }

    let mut events = Vec::with_capacity((body.len() / 4) * 2 + 1);
    let mut wait_ticks = 0u64;
    let mut writes = 0usize;

    for command in body.chunks_exact(4) {
        let reg = command[0] as u16;
        let value = command[1];
        let delay = u16::from_le_bytes([command[2], command[3]]) as u64;
        events.push(Event::Write { reg, value });
        writes += 1;
        if delay != 0 {
            events.push(Event::Wait(delay));
            wait_ticks += delay;
        }
    }

    events.push(Event::End);
    Ok(Song {
        format: Format::Imf,
        events,
        timebase_rate: imf_rate,
        writes,
        wait_ticks,
    })
}

fn parse_vgm(data: &[u8]) -> Result<Song> {
    if data.len() < 0x40 || &data[..4] != b"Vgm " {
        return Err(PlayerError::new("not a VGM file"));
    }

    let version = read_u32_at(data, 0x08)?;
    let eof = (4usize).saturating_add(read_u32_at(data, 0x04)? as usize);
    let gd3 = match read_u32_at(data, 0x14)? {
        0 => eof,
        offset => (0x14usize).saturating_add(offset as usize),
    };
    let data_offset = if version < 0x150 {
        0x40
    } else {
        match read_u32_at(data, 0x34)? {
            0 => 0x40,
            offset => (0x34usize).saturating_add(offset as usize),
        }
    };
    let end = eof.min(gd3).min(data.len());

    if data_offset >= end {
        return Err(PlayerError::new(format!(
            "VGM data offset 0x{data_offset:x} is past end 0x{end:x}"
        )));
    }

    let mut idx = data_offset;
    let mut events = Vec::new();
    let mut wait_ticks = 0u64;
    let mut writes = 0usize;

    while idx < end {
        let command_offset = idx;
        let command = data[idx];
        idx += 1;

        match command {
            0x66 => {
                events.push(Event::End);
                return Ok(Song {
                    format: Format::Vgm,
                    events,
                    timebase_rate: VGM_RATE,
                    writes,
                    wait_ticks,
                });
            }
            0x61 => {
                ensure_bytes(data, idx, 2, command_offset)?;
                let wait = u16::from_le_bytes([data[idx], data[idx + 1]]) as u64;
                idx += 2;
                push_wait(&mut events, &mut wait_ticks, wait);
            }
            0x62 => push_wait(&mut events, &mut wait_ticks, 735),
            0x63 => push_wait(&mut events, &mut wait_ticks, 882),
            0x70..=0x7f => push_wait(&mut events, &mut wait_ticks, (command & 0x0f) as u64 + 1),
            0x80..=0x8f => push_wait(&mut events, &mut wait_ticks, (command & 0x0f) as u64),
            0x5a | 0x5e => {
                ensure_bytes(data, idx, 2, command_offset)?;
                let reg = data[idx] as u16;
                let value = data[idx + 1];
                idx += 2;
                events.push(Event::Write { reg, value });
                writes += 1;
            }
            0x5f => {
                ensure_bytes(data, idx, 2, command_offset)?;
                let reg = data[idx] as u16 | 0x100;
                let value = data[idx + 1];
                idx += 2;
                events.push(Event::Write { reg, value });
                writes += 1;
            }
            0x30 | 0x31 | 0x4f | 0x50 => skip(data, &mut idx, 1, command_offset)?,
            0x51..=0x5d | 0xa0..=0xbf => skip(data, &mut idx, 2, command_offset)?,
            0xc0..=0xdf => skip(data, &mut idx, 3, command_offset)?,
            0xe0..=0xff => skip(data, &mut idx, 4, command_offset)?,
            0x67 => {
                ensure_bytes(data, idx, 6, command_offset)?;
                if data[idx] != 0x66 {
                    return Err(PlayerError::new(format!(
                        "malformed VGM data block at 0x{command_offset:x}"
                    )));
                }
                let size = read_u32_at(data, idx + 2)? as usize;
                idx += 6;
                skip(data, &mut idx, size, command_offset)?;
            }
            0x68 => skip(data, &mut idx, 11, command_offset)?,
            0x90 | 0x91 => skip(data, &mut idx, 4, command_offset)?,
            0x92 => skip(data, &mut idx, 5, command_offset)?,
            0x93 => skip(data, &mut idx, 10, command_offset)?,
            0x94 => skip(data, &mut idx, 1, command_offset)?,
            0x95 => skip(data, &mut idx, 4, command_offset)?,
            _ => {
                return Err(PlayerError::new(format!(
                    "unsupported VGM command 0x{command:02x} at 0x{command_offset:x}"
                )));
            }
        }
    }

    events.push(Event::End);
    Ok(Song {
        format: Format::Vgm,
        events,
        timebase_rate: VGM_RATE,
        writes,
        wait_ticks,
    })
}

fn push_wait(events: &mut Vec<Event>, wait_ticks: &mut u64, wait: u64) {
    if wait != 0 {
        events.push(Event::Wait(wait));
        *wait_ticks += wait;
    }
}

fn parse_dro(data: &[u8]) -> Result<Song> {
    if data.len() < 0x18 || &data[..8] != b"DBRAWOPL" {
        return Err(PlayerError::new("not a DRO file"));
    }
    // v0.1 packs the version as a u32 = 0x00010000 at offset 0x08; v2.0 puts a
    // u32 versionMajor=2 there. Anything else is a malformed or post-v2 file.
    let version_word = u32::from_le_bytes([data[0x08], data[0x09], data[0x0a], data[0x0b]]);
    match version_word {
        0x0001_0000 => parse_dro_v1(data),
        2 => parse_dro_v2(data),
        _ => Err(PlayerError::new(format!(
            "unsupported DRO version word 0x{version_word:08x}"
        ))),
    }
}

fn parse_dro_v1(data: &[u8]) -> Result<Song> {
    // v0.1 layout: 24-byte header, then variable-length commands.
    //   0x00 "DBRAWOPL"; 0x08 u32 version (0x00010000); 0x0c u32 lengthMs;
    //   0x10 u32 lengthBytes; 0x14 u8 hardware; 0x15-0x17 format/compression/pad;
    //   0x18 data starts. Body: 0x00 nn = (nn+1)ms delay; 0x01 lo hi = (lo|hi<<8)+1
    //   ms delay; 0x02 = bank low; 0x03 = bank high; 0x04 RR VV = escape literal
    //   write; otherwise (RR>=0x05) RR VV = normal write.
    if data.len() < 0x18 {
        return Err(PlayerError::new("truncated DRO v0.1 header"));
    }
    let length_bytes =
        u32::from_le_bytes([data[0x10], data[0x11], data[0x12], data[0x13]]) as usize;
    let body_start = 0x18usize;
    let body_end = body_start.saturating_add(length_bytes).min(data.len());

    let mut events = Vec::new();
    let mut wait_ticks = 0u64;
    let mut writes = 0usize;
    let mut bank: u16 = 0;
    let mut idx = body_start;

    while idx < body_end {
        let cmd = data[idx];
        idx += 1;
        match cmd {
            0x00 => {
                if idx >= body_end {
                    break;
                }
                push_wait(&mut events, &mut wait_ticks, data[idx] as u64 + 1);
                idx += 1;
            }
            0x01 => {
                if idx + 1 >= body_end {
                    break;
                }
                let delay = u16::from_le_bytes([data[idx], data[idx + 1]]) as u64 + 1;
                push_wait(&mut events, &mut wait_ticks, delay);
                idx += 2;
            }
            0x02 => bank = 0,
            0x03 => bank = 0x100,
            0x04 => {
                if idx + 1 >= body_end {
                    break;
                }
                let reg = data[idx] as u16;
                let value = data[idx + 1];
                idx += 2;
                events.push(Event::Write {
                    reg: bank | reg,
                    value,
                });
                writes += 1;
            }
            reg => {
                if idx >= body_end {
                    break;
                }
                let value = data[idx];
                idx += 1;
                events.push(Event::Write {
                    reg: bank | reg as u16,
                    value,
                });
                writes += 1;
            }
        }
    }

    events.push(Event::End);
    Ok(Song {
        format: Format::Dro,
        events,
        timebase_rate: DRO_RATE,
        writes,
        wait_ticks,
    })
}

fn parse_dro_v2(data: &[u8]) -> Result<Song> {
    // v2.0 layout: u16 versionHi=2 at 0x08, u16 versionLo=0 at 0x0a (read here
    // as a single u32 = 2); u32 commands (pair count) at 0x0c; u32 lengthMs at
    // 0x10; hardware/format/compression at 0x14-0x16; shortDelay/longDelay/
    // codemapLength at 0x17/0x18/0x19; codemap at 0x1a; data after.
    if data.len() < 0x1a {
        return Err(PlayerError::new("truncated DRO v2 header"));
    }
    let length_pairs =
        u32::from_le_bytes([data[0x0c], data[0x0d], data[0x0e], data[0x0f]]) as usize;
    let format = data[0x15];
    let compression = data[0x16];
    let short_delay_code = data[0x17];
    let long_delay_code = data[0x18];
    let codemap_length = data[0x19] as usize;

    if format != 0 {
        return Err(PlayerError::new(format!(
            "DRO v2 unsupported format {format}"
        )));
    }
    if compression != 0 {
        return Err(PlayerError::new(format!(
            "DRO v2 unsupported compression {compression}"
        )));
    }
    if codemap_length > 0x80 {
        return Err(PlayerError::new(format!(
            "DRO v2 codemap length {codemap_length} exceeds 0x80"
        )));
    }

    let codemap_start = 0x1a;
    let codemap_end = codemap_start + codemap_length;
    if codemap_end > data.len() {
        return Err(PlayerError::new("truncated DRO v2 codemap"));
    }
    let codemap = &data[codemap_start..codemap_end];

    let data_start = codemap_end;
    let data_end = data_start
        .checked_add(length_pairs.saturating_mul(2))
        .ok_or_else(|| PlayerError::new("DRO v2 length_pairs overflow"))?
        .min(data.len());

    let mut events = Vec::with_capacity(length_pairs);
    let mut wait_ticks = 0u64;
    let mut writes = 0usize;

    let mut idx = data_start;
    while idx + 1 < data_end {
        let code = data[idx];
        let value = data[idx + 1];
        idx += 2;

        if code == short_delay_code {
            push_wait(&mut events, &mut wait_ticks, value as u64 + 1);
        } else if code == long_delay_code {
            push_wait(&mut events, &mut wait_ticks, (value as u64 + 1) * 256);
        } else {
            let bank = u16::from((code & 0x80) != 0) << 8;
            let map_idx = (code & 0x7f) as usize;
            if map_idx >= codemap.len() {
                return Err(PlayerError::new(format!(
                    "DRO v2 code 0x{code:02x} out of codemap range"
                )));
            }
            let reg = bank | codemap[map_idx] as u16;
            events.push(Event::Write { reg, value });
            writes += 1;
        }
    }

    events.push(Event::End);
    Ok(Song {
        format: Format::Dro,
        events,
        timebase_rate: DRO_RATE,
        writes,
        wait_ticks,
    })
}

fn skip(data: &[u8], idx: &mut usize, count: usize, command_offset: usize) -> Result<()> {
    ensure_bytes(data, *idx, count, command_offset)?;
    *idx += count;
    Ok(())
}

fn ensure_bytes(data: &[u8], offset: usize, count: usize, command_offset: usize) -> Result<()> {
    if offset
        .checked_add(count)
        .is_some_and(|end| end <= data.len())
    {
        Ok(())
    } else {
        Err(PlayerError::new(format!(
            "truncated VGM command at 0x{command_offset:x}"
        )))
    }
}

fn read_u16_at(data: &[u8], offset: usize) -> Result<u16> {
    ensure_header_bytes(data, offset, 2)?;
    Ok(u16::from_le_bytes([data[offset], data[offset + 1]]))
}

fn read_u32_at(data: &[u8], offset: usize) -> Result<u32> {
    ensure_header_bytes(data, offset, 4)?;
    Ok(u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ]))
}

fn ensure_header_bytes(data: &[u8], offset: usize, count: usize) -> Result<()> {
    if offset
        .checked_add(count)
        .is_some_and(|end| end <= data.len())
    {
        Ok(())
    } else {
        Err(PlayerError::new("file header is truncated"))
    }
}

#[derive(Default)]
struct LiveStatus {
    frames_rendered: AtomicU64,
    frames_queued: AtomicU64,
    active_voices: AtomicUsize,
}

impl LiveStatus {
    fn record_frame(&self, frames_rendered: u64, active_voices: Option<usize>) {
        self.frames_rendered
            .store(frames_rendered, Ordering::Relaxed);
        if let Some(active_voices) = active_voices {
            self.active_voices.store(active_voices, Ordering::Relaxed);
        }
    }

    fn frames_rendered(&self) -> u64 {
        self.frames_rendered.load(Ordering::Relaxed)
    }

    fn record_queued_frames(&self, frames_queued: u64) {
        self.frames_queued.store(frames_queued, Ordering::Relaxed);
    }

    fn frames_queued(&self) -> u64 {
        self.frames_queued.load(Ordering::Relaxed)
    }

    fn active_voices(&self) -> usize {
        self.active_voices.load(Ordering::Relaxed)
    }
}

struct StatusPrinter {
    enabled: bool,
    tty: bool,
    sample_rate: u32,
    last_emit: Instant,
    last_frames: u64,
    playback_start: Option<Instant>,
}

impl StatusPrinter {
    fn new(enabled: bool, sample_rate: u32) -> Self {
        Self {
            enabled,
            tty: io::stderr().is_terminal(),
            sample_rate,
            last_emit: Instant::now() - STATUS_INTERVAL,
            last_frames: 0,
            playback_start: None,
        }
    }

    fn start_playback(&mut self, status: &LiveStatus, start: Instant) {
        self.playback_start = Some(start);
        self.last_emit = start - STATUS_INTERVAL;
        self.last_frames = status.frames_rendered();
    }

    fn playback(&mut self, status: &LiveStatus) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        if now.duration_since(self.last_emit) < STATUS_INTERVAL {
            return;
        }

        let rendered_frames = status.frames_rendered();
        let queued_frames = status.frames_queued();
        let audio_s = queued_frames as f64 / self.sample_rate as f64;
        let elapsed = self
            .playback_start
            .map(|start| now.duration_since(start).as_secs_f64())
            .unwrap_or(0.0);
        let d_wall = now.duration_since(self.last_emit).as_secs_f64();
        let d_audio =
            rendered_frames.saturating_sub(self.last_frames) as f64 / self.sample_rate as f64;
        let rt = if d_wall > 0.0 { d_audio / d_wall } else { 0.0 };
        let buf_ahead = audio_s - elapsed;
        let active = status.active_voices();

        self.write_line(&format!(
            "[{audio_s:6.1}s] {active:2}/36 voices  RT {rt:4.2}x  buf {buf_ahead:+5.2}s"
        ));
        self.last_emit = now;
        self.last_frames = rendered_frames;
    }

    fn finish(&self) {
        if self.enabled && self.tty {
            let _ = writeln!(io::stderr());
        }
    }

    fn write_line(&self, line: &str) {
        let mut stderr = io::stderr().lock();
        if self.tty {
            let _ = write!(stderr, "\r{line}   ");
        } else {
            let _ = writeln!(stderr, "{line}");
        }
        let _ = stderr.flush();
    }
}

fn playahead_target_frames(opts: &Options, sample_rate: u32) -> u64 {
    let target = opts.prebuffer_ms.saturating_mul(sample_rate as u64) / 1000;
    target.max(opts.chunk_frames as u64)
}

fn frames_to_ms(frames: u64, sample_rate: u32) -> u64 {
    frames.saturating_mul(1000) / sample_rate as u64
}

fn should_queue_more(
    playback_start: Option<Instant>,
    frames_queued: u64,
    sample_rate: u32,
    playahead_target: u64,
) -> bool {
    match playback_start {
        Some(start) => {
            estimated_buffer_frames(frames_queued, sample_rate, start) < playahead_target
        }
        None => true,
    }
}

fn stereo_frame_count(samples: &[f32]) -> u64 {
    (samples.len() / 2) as u64
}

fn spawn_render_thread(
    mut source: OplFileSource,
    chunk_frames: usize,
) -> (
    mpsc::Receiver<Option<Vec<f32>>>,
    std::thread::JoinHandle<()>,
) {
    let (tx, rx) = mpsc::sync_channel(RENDER_QUEUE_CHUNKS);
    let handle = std::thread::spawn(move || loop {
        let chunk = render_chunk(&mut source, chunk_frames);
        if chunk.is_empty() {
            let _ = tx.send(None);
            break;
        }
        if tx.send(Some(chunk)).is_err() {
            break;
        }
    });
    (rx, handle)
}

fn render_chunk(source: &mut OplFileSource, frames: usize) -> Vec<f32> {
    let mut samples = Vec::with_capacity(frames * 2);
    for _ in 0..frames {
        let Some(left) = source.next() else {
            break;
        };
        let Some(right) = source.next() else {
            samples.push(left);
            break;
        };
        samples.push(left);
        samples.push(right);
    }
    samples
}

fn estimated_buffer_frames(frames_queued: u64, sample_rate: u32, playback_start: Instant) -> u64 {
    let elapsed_frames = playback_start
        .elapsed()
        .as_secs_f64()
        .mul_add(sample_rate as f64, 0.0) as u64;
    frames_queued.saturating_sub(elapsed_frames)
}

struct OplFileSource {
    chip: Opl3Chip,
    events: Vec<Event>,
    event_index: usize,
    timebase_rate: u32,
    sample_rate: u32,
    buffered: bool,
    frame_wait: u64,
    timing_remainder: u64,
    current_frame: [f32; 2],
    current_channel: usize,
    frames_rendered: u64,
    status: Option<Arc<LiveStatus>>,
    track_active_voices: bool,
    tail_frames: u64,
}

impl OplFileSource {
    fn new(
        events: Vec<Event>,
        timebase_rate: u32,
        sample_rate: u32,
        buffered: bool,
        tail_ms: u64,
    ) -> Self {
        Self {
            chip: Opl3Chip::new(sample_rate),
            events,
            event_index: 0,
            timebase_rate,
            sample_rate,
            buffered,
            frame_wait: 0,
            timing_remainder: 0,
            current_frame: [0.0, 0.0],
            current_channel: 0,
            frames_rendered: 0,
            status: None,
            track_active_voices: false,
            tail_frames: tail_ms.saturating_mul(sample_rate as u64) / 1000,
        }
    }

    fn with_status(mut self, status: Option<Arc<LiveStatus>>, track_active_voices: bool) -> Self {
        self.status = status;
        self.track_active_voices = track_active_voices;
        self
    }

    fn render_next_frame(&mut self) -> bool {
        while self.frame_wait == 0 {
            let Some(event) = self.events.get(self.event_index).cloned() else {
                if self.start_tail_or_finish() {
                    continue;
                }
                return false;
            };
            self.event_index += 1;

            match event {
                Event::Write { reg, value } => {
                    if self.buffered {
                        self.chip.write_register_buffered(reg, value);
                    } else {
                        self.chip.write_register(reg, value);
                    }
                }
                Event::Wait(ticks) => {
                    self.frame_wait = self.convert_wait_ticks(ticks);
                }
                Event::End => {
                    if self.start_tail_or_finish() {
                        continue;
                    }
                    return false;
                }
            }
        }

        let mut sample = [0i16; 2];
        if self.chip.generate_resampled(&mut sample).is_err() {
            return false;
        }
        self.current_frame = [
            sample[0] as f32 / i16::MAX as f32,
            sample[1] as f32 / i16::MAX as f32,
        ];
        self.frame_wait -= 1;
        self.frames_rendered += 1;
        if let Some(status) = &self.status {
            let active_voices = self
                .track_active_voices
                .then(|| self.chip.active_voice_count());
            status.record_frame(self.frames_rendered, active_voices);
        }
        true
    }

    fn convert_wait_ticks(&mut self, ticks: u64) -> u64 {
        let numerator = ticks
            .saturating_mul(self.sample_rate as u64)
            .saturating_add(self.timing_remainder);
        self.timing_remainder = numerator % self.timebase_rate as u64;
        numerator / self.timebase_rate as u64
    }

    fn start_tail_or_finish(&mut self) -> bool {
        if self.tail_frames == 0 {
            false
        } else {
            self.frame_wait = self.tail_frames;
            self.tail_frames = 0;
            true
        }
    }
}

impl Iterator for OplFileSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current_channel == 0 && !self.render_next_frame() {
            return None;
        }

        let sample = self.current_frame[self.current_channel];
        self.current_channel = (self.current_channel + 1) % 2;
        Some(sample)
    }
}

impl Source for OplFileSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> NonZero<u16> {
        NonZero::new(2).unwrap()
    }

    fn sample_rate(&self) -> NonZero<u32> {
        NonZero::new(self.sample_rate).unwrap()
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

#[cfg(target_os = "linux")]
mod pulse {
    use std::ffi::{c_char, c_int, c_void, CStr, CString};
    use std::mem;
    use std::ptr;

    use super::{PlayerError, Result};

    const RTLD_NOW: c_int = 2;
    const PA_STREAM_PLAYBACK: c_int = 1;
    const PA_SAMPLE_FLOAT32LE: c_int = 5;

    #[repr(C)]
    struct PaSampleSpec {
        format: c_int,
        rate: u32,
        channels: u8,
    }

    type PaSimple = c_void;
    type PaSimpleNew = unsafe extern "C" fn(
        *const c_char,
        *const c_char,
        c_int,
        *const c_char,
        *const c_char,
        *const PaSampleSpec,
        *const c_void,
        *const c_void,
        *mut c_int,
    ) -> *mut PaSimple;
    type PaSimpleFree = unsafe extern "C" fn(*mut PaSimple);
    type PaSimpleWrite =
        unsafe extern "C" fn(*mut PaSimple, *const c_void, usize, *mut c_int) -> c_int;
    type PaSimpleDrain = unsafe extern "C" fn(*mut PaSimple, *mut c_int) -> c_int;

    #[link(name = "dl")]
    unsafe extern "C" {
        fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        fn dlclose(handle: *mut c_void) -> c_int;
        fn dlerror() -> *const c_char;
    }

    struct PulseLib {
        handle: *mut c_void,
        simple_new: PaSimpleNew,
        simple_free: PaSimpleFree,
        simple_write: PaSimpleWrite,
        simple_drain: PaSimpleDrain,
    }

    impl PulseLib {
        fn load() -> Result<Self> {
            let library_name = CString::new("libpulse-simple.so.0").unwrap();
            let handle = unsafe { dlopen(library_name.as_ptr(), RTLD_NOW) };
            if handle.is_null() {
                return Err(PlayerError::new(format!(
                    "failed to load libpulse-simple.so.0: {}",
                    dl_error()
                )));
            }

            let lib = unsafe {
                Self {
                    handle,
                    simple_new: symbol(handle, b"pa_simple_new\0")?,
                    simple_free: symbol(handle, b"pa_simple_free\0")?,
                    simple_write: symbol(handle, b"pa_simple_write\0")?,
                    simple_drain: symbol(handle, b"pa_simple_drain\0")?,
                }
            };
            Ok(lib)
        }
    }

    impl Drop for PulseLib {
        fn drop(&mut self) {
            unsafe {
                dlclose(self.handle);
            }
        }
    }

    unsafe fn symbol<T: Copy>(handle: *mut c_void, name: &'static [u8]) -> Result<T> {
        let ptr = dlsym(handle, name.as_ptr().cast());
        if ptr.is_null() {
            return Err(PlayerError::new(format!(
                "failed to load PulseAudio symbol {}: {}",
                CStr::from_bytes_with_nul(name).unwrap().to_string_lossy(),
                dl_error()
            )));
        }
        Ok(mem::transmute_copy(&ptr))
    }

    fn dl_error() -> String {
        let err = unsafe { dlerror() };
        if err.is_null() {
            String::from("unknown dlopen error")
        } else {
            unsafe { CStr::from_ptr(err) }
                .to_string_lossy()
                .into_owned()
        }
    }

    pub(super) struct PulseStream {
        lib: PulseLib,
        stream: *mut PaSimple,
    }

    impl PulseStream {
        pub(super) fn new(
            sample_rate: u32,
            device: Option<&str>,
            stream_name: &str,
        ) -> Result<Self> {
            let lib = PulseLib::load()?;
            let app_name = CString::new("nuked-opl3 play_file")
                .map_err(|_| PlayerError::new("PulseAudio app name contains a NUL byte"))?;
            let stream_name = CString::new(stream_name)
                .map_err(|_| PlayerError::new("PulseAudio stream name contains a NUL byte"))?;
            let server = pulse_server_name()?;
            let device = device
                .map(|device| {
                    CString::new(device)
                        .map_err(|_| PlayerError::new("PulseAudio device name contains a NUL byte"))
                })
                .transpose()?;
            let sample_spec = PaSampleSpec {
                format: PA_SAMPLE_FLOAT32LE,
                rate: sample_rate,
                channels: 2,
            };

            let mut error = 0;
            let stream = unsafe {
                (lib.simple_new)(
                    server
                        .as_ref()
                        .map_or(ptr::null(), |server| server.as_ptr()),
                    app_name.as_ptr(),
                    PA_STREAM_PLAYBACK,
                    device
                        .as_ref()
                        .map_or(ptr::null(), |device| device.as_ptr()),
                    stream_name.as_ptr(),
                    &sample_spec,
                    ptr::null(),
                    ptr::null(),
                    &mut error,
                )
            };
            if stream.is_null() {
                return Err(PlayerError::new(format!(
                    "failed to connect to PulseAudio server: {} (code {error}); PULSE_SERVER={:?}",
                    pulse_error(error),
                    std::env::var_os("PULSE_SERVER")
                )));
            }

            Ok(Self { lib, stream })
        }

        pub(super) fn write_f32(&mut self, samples: &[f32]) -> Result<()> {
            let mut error = 0;
            let result = unsafe {
                (self.lib.simple_write)(
                    self.stream,
                    samples.as_ptr().cast(),
                    mem::size_of_val(samples),
                    &mut error,
                )
            };
            if result < 0 {
                Err(PlayerError::new(format!(
                    "failed to write to PulseAudio stream: {} (code {error})",
                    pulse_error(error)
                )))
            } else {
                Ok(())
            }
        }

        pub(super) fn drain(&mut self) -> Result<()> {
            let mut error = 0;
            let result = unsafe { (self.lib.simple_drain)(self.stream, &mut error) };
            if result < 0 {
                Err(PlayerError::new(format!(
                    "failed to drain PulseAudio stream: {} (code {error})",
                    pulse_error(error)
                )))
            } else {
                Ok(())
            }
        }
    }

    impl Drop for PulseStream {
        fn drop(&mut self) {
            if !self.stream.is_null() {
                unsafe {
                    (self.lib.simple_free)(self.stream);
                }
            }
        }
    }

    fn pulse_server_name() -> Result<Option<CString>> {
        let server = std::env::var_os("PULSE_SERVER")
            .and_then(|server| server.into_string().ok())
            .or_else(|| {
                std::path::Path::new("/mnt/wslg/PulseServer")
                    .exists()
                    .then(|| String::from("unix:/mnt/wslg/PulseServer"))
            });

        server
            .map(|server| {
                CString::new(server)
                    .map_err(|_| PlayerError::new("PULSE_SERVER contains a NUL byte"))
            })
            .transpose()
    }

    fn pulse_error(error: c_int) -> &'static str {
        match error {
            0 => "OK",
            1 => "access failure",
            2 => "unknown command",
            3 => "invalid argument",
            4 => "entity exists",
            5 => "no such entity",
            6 => "connection refused",
            7 => "protocol error",
            8 => "timeout",
            9 => "no authentication key",
            10 => "internal error",
            11 => "connection terminated",
            12 => "entity killed",
            13 => "invalid server",
            14 => "module initialization failed",
            15 => "bad state",
            16 => "no data",
            17 => "incompatible protocol version",
            18 => "data too large",
            19 => "operation not supported",
            20 => "unknown error",
            21 => "extension does not exist",
            22 => "obsolete functionality",
            23 => "missing implementation",
            24 => "forked without exec",
            25 => "I/O error",
            26 => "device or resource busy",
            _ => "unknown PulseAudio error",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_raw_imf_commands() {
        let song = parse_imf(&[0x20, 0x01, 0x05, 0x00], 700).unwrap();
        assert_eq!(song.format, Format::Imf);
        assert_eq!(song.timebase_rate, 700);
        assert_eq!(song.writes, 1);
        assert_eq!(song.wait_ticks, 5);
        assert_eq!(
            song.events,
            vec![
                Event::Write {
                    reg: 0x20,
                    value: 0x01,
                },
                Event::Wait(5),
                Event::End,
            ]
        );
    }

    #[test]
    fn parses_length_prefixed_imf_commands() {
        let song = parse_imf(&[0x04, 0x00, 0x20, 0x01, 0x05, 0x00], 700).unwrap();
        assert_eq!(song.events.len(), 3);
        assert_eq!(song.writes, 1);
    }

    #[test]
    fn parses_minimal_vgm_opl3_stream() {
        let mut data = vec![0; 0x40];
        data[0..4].copy_from_slice(b"Vgm ");
        data[0x08..0x0c].copy_from_slice(&0x0000_0150u32.to_le_bytes());
        data[0x34..0x38].copy_from_slice(&0x0cu32.to_le_bytes());
        data.extend_from_slice(&[0x5e, 0x20, 0x01, 0x61, 0x10, 0x00, 0x5f, 0x40, 0x02, 0x66]);
        let eof = (data.len() - 4) as u32;
        data[0x04..0x08].copy_from_slice(&eof.to_le_bytes());

        let song = parse_vgm(&data).unwrap();
        assert_eq!(song.format, Format::Vgm);
        assert_eq!(song.timebase_rate, VGM_RATE);
        assert_eq!(song.writes, 2);
        assert_eq!(song.wait_ticks, 0x10);
        assert_eq!(
            song.events,
            vec![
                Event::Write {
                    reg: 0x20,
                    value: 0x01,
                },
                Event::Wait(0x10),
                Event::Write {
                    reg: 0x140,
                    value: 0x02,
                },
                Event::End,
            ]
        );
    }

    #[test]
    fn vgm_defaults_to_buffered_writes() {
        let opts = test_options(false, false);
        assert!(effective_buffered(&opts, Format::Vgm));
    }

    #[test]
    fn imf_defaults_to_synchronous_writes() {
        let opts = test_options(false, false);
        assert!(!effective_buffered(&opts, Format::Imf));
    }

    #[test]
    fn sync_writes_overrides_vgm_buffered_default() {
        let opts = test_options(false, true);
        assert!(!effective_buffered(&opts, Format::Vgm));
    }

    #[test]
    fn explicit_buffered_still_applies_to_imf() {
        let opts = test_options(true, false);
        assert!(effective_buffered(&opts, Format::Imf));
    }

    #[test]
    fn rejects_conflicting_write_mode_options() {
        let err = parse_args(
            ["--buffered", "--sync-writes", "song.vgm"]
                .into_iter()
                .map(std::ffi::OsString::from),
        )
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("--buffered and --sync-writes cannot be used together"));
    }

    #[test]
    fn parses_live_buffer_options() {
        let opts = parse_args(
            [
                "--status",
                "--prebuffer-ms",
                "1500",
                "--chunk-frames",
                "2048",
                "song.vgm",
            ]
            .into_iter()
            .map(std::ffi::OsString::from),
        )
        .unwrap();

        assert!(opts.status);
        assert_eq!(opts.prebuffer_ms, 1500);
        assert_eq!(opts.chunk_frames, 2048);
    }

    #[test]
    fn rejects_zero_chunk_frames() {
        let err = parse_args(
            ["--chunk-frames", "0", "song.vgm"]
                .into_iter()
                .map(std::ffi::OsString::from),
        )
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("--chunk-frames must be greater than zero"));
    }

    #[test]
    fn vgm_default_buffering_changes_dense_write_render() {
        let mut opts = test_options(false, false);
        opts.buffered = effective_buffered(&opts, Format::Vgm);
        let buffered = source_checksum(OplFileSource::new(
            simple_tone_events(),
            49_716,
            49_716,
            opts.buffered,
            0,
        ));

        let mut opts = test_options(false, true);
        opts.buffered = effective_buffered(&opts, Format::Vgm);
        let synchronous = source_checksum(OplFileSource::new(
            simple_tone_events(),
            49_716,
            49_716,
            opts.buffered,
            0,
        ));

        assert_ne!(buffered, synchronous);
    }

    #[test]
    fn source_converts_waits_with_remainder() {
        let mut source = OplFileSource::new(vec![Event::Wait(1), Event::Wait(1)], 3, 2, false, 0);
        assert_eq!(source.convert_wait_ticks(1), 0);
        assert_eq!(source.convert_wait_ticks(1), 1);
        assert_eq!(source.convert_wait_ticks(1), 1);
    }

    #[test]
    fn chunked_render_preserves_source_stream() {
        let events = simple_tone_events();
        let full = source_checksum(OplFileSource::new(events.clone(), 49_716, 49_716, false, 0));
        let mut source = OplFileSource::new(events, 49_716, 49_716, false, 0);
        let mut chunks = Vec::new();
        loop {
            let chunk = render_chunk(&mut source, 100);
            if chunk.is_empty() {
                break;
            }
            chunks.extend(chunk);
        }

        assert_eq!(full, sample_checksum(chunks));
    }

    fn test_options(buffered: bool, sync_writes: bool) -> Options {
        Options {
            path: None,
            chunk_frames: DEFAULT_CHUNK_FRAMES,
            imf_rate: DEFAULT_IMF_RATE,
            backend: Backend::Auto,
            buffered,
            device: None,
            dry_run: false,
            list_devices: false,
            prebuffer_ms: DEFAULT_PREBUFFER_MS,
            sample_rate: DEFAULT_WAV_RATE,
            status: false,
            sync_writes,
            tail_ms: 0,
            wav_output: None,
        }
    }

    fn simple_tone_events() -> Vec<Event> {
        vec![
            Event::Write {
                reg: 0x105,
                value: 0x01,
            },
            Event::Write {
                reg: 0x020,
                value: 0x21,
            },
            Event::Write {
                reg: 0x040,
                value: 0x10,
            },
            Event::Write {
                reg: 0x060,
                value: 0xf0,
            },
            Event::Write {
                reg: 0x080,
                value: 0x0f,
            },
            Event::Write {
                reg: 0x023,
                value: 0x21,
            },
            Event::Write {
                reg: 0x043,
                value: 0x00,
            },
            Event::Write {
                reg: 0x063,
                value: 0xf0,
            },
            Event::Write {
                reg: 0x083,
                value: 0x0f,
            },
            Event::Write {
                reg: 0x0a0,
                value: 0x44,
            },
            Event::Write {
                reg: 0x0c0,
                value: 0x31,
            },
            Event::Write {
                reg: 0x0b0,
                value: 0x32,
            },
            Event::Wait(4096),
            Event::Write {
                reg: 0x0b0,
                value: 0x12,
            },
            Event::Wait(1024),
            Event::End,
        ]
    }

    fn source_checksum(source: OplFileSource) -> u64 {
        sample_checksum(source)
    }

    fn sample_checksum(samples: impl IntoIterator<Item = f32>) -> u64 {
        samples
            .into_iter()
            .fold(0xcbf29ce484222325, |checksum, sample| {
                checksum
                    .wrapping_mul(0x100000001b3)
                    .wrapping_add(sample.to_bits() as u64)
            })
    }
}
