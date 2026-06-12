use super::*;
use crate::bindings;
use std::hint::black_box;
use std::time::{Duration, Instant};

const CHIP_RATE: u32 = 49_716;
const CRYSTAL_OSCILLATOR_TRACE: &str =
    include_str!("../../tests/fixtures/crystal_oscillator_20k.trace");
const INTERMISSION_IMF_TRACE: &str =
    include_str!("../../tests/fixtures/intermission_fuck_yeah_imf_20k.trace");
const DEFAULT_IMF_RATE: u32 = 700;
const C_CHIP_STORAGE_SIZE: usize = 32 * 1024;

#[repr(align(8))]
struct CChipStorage([u8; C_CHIP_STORAGE_SIZE]);

impl CChipStorage {
    fn new() -> Self {
        Self([0; C_CHIP_STORAGE_SIZE])
    }

    fn as_mut_ptr(&mut self) -> *mut bindings::Opl3Chip {
        self.0.as_mut_ptr().cast()
    }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Write(u16, u8),
    Samples(usize),
}

fn parse_trace(trace: &str) -> Vec<Op> {
    let mut ops = Vec::new();
    for (idx, raw_line) in trace.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut parts = line.split_whitespace();
        let op = parts.next().expect("non-empty trace line");
        match op {
            "W" => {
                let reg = parts
                    .next()
                    .unwrap_or_else(|| panic!("trace line {} missing register", idx + 1));
                let value = parts
                    .next()
                    .unwrap_or_else(|| panic!("trace line {} missing value", idx + 1));
                assert!(
                    parts.next().is_none(),
                    "trace line {} has trailing fields",
                    idx + 1
                );
                ops.push(Op::Write(
                    u16::from_str_radix(reg, 16)
                        .unwrap_or_else(|_| panic!("trace line {} has bad register", idx + 1)),
                    u8::from_str_radix(value, 16)
                        .unwrap_or_else(|_| panic!("trace line {} has bad value", idx + 1)),
                ));
            }
            "S" => {
                let count = parts
                    .next()
                    .unwrap_or_else(|| panic!("trace line {} missing sample count", idx + 1));
                assert!(
                    parts.next().is_none(),
                    "trace line {} has trailing fields",
                    idx + 1
                );
                ops.push(Op::Samples(count.parse().unwrap_or_else(|_| {
                    panic!("trace line {} has bad sample count", idx + 1)
                })));
            }
            "E" => break,
            _ => panic!("trace line {} has unknown op {op:?}", idx + 1),
        }
    }
    ops
}

fn parse_vgm(data: &[u8]) -> Result<Vec<Op>, String> {
    const VGM_RATE: u64 = 44_100;

    if data.len() < 0x40 || &data[..4] != b"Vgm " {
        return Err("not a VGM file".to_string());
    }

    let read_u16 = |offset: usize| -> Result<u16, String> {
        let bytes = data
            .get(offset..offset + 2)
            .ok_or_else(|| format!("truncated VGM at offset 0x{offset:x}"))?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    };
    let read_u32 = |offset: usize| -> Result<u32, String> {
        let bytes = data
            .get(offset..offset + 4)
            .ok_or_else(|| format!("truncated VGM at offset 0x{offset:x}"))?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };
    let ensure = |offset: usize, count: usize| -> Result<(), String> {
        if offset
            .checked_add(count)
            .is_some_and(|end| end <= data.len())
        {
            Ok(())
        } else {
            Err(format!(
                "truncated VGM command payload at offset 0x{offset:x}"
            ))
        }
    };

    let version = read_u32(0x08)?;
    let eof = 4usize.saturating_add(read_u32(0x04)? as usize);
    let gd3 = match read_u32(0x14)? {
        0 => eof,
        offset => 0x14usize.saturating_add(offset as usize),
    };
    let data_offset = if version < 0x150 {
        0x40
    } else {
        match read_u32(0x34)? {
            0 => 0x40,
            offset => 0x34usize.saturating_add(offset as usize),
        }
    };
    let end = eof.min(gd3).min(data.len());
    if data_offset >= end {
        return Err(format!(
            "VGM data offset 0x{data_offset:x} is past end 0x{end:x}"
        ));
    }

    let mut ops = Vec::new();
    let mut idx = data_offset;
    let mut pending_wait_44k = 0u64;
    let mut bresenham_remainder = 0u64;

    fn flush_wait(ops: &mut Vec<Op>, pending_wait_44k: &mut u64, bresenham_remainder: &mut u64) {
        if *pending_wait_44k == 0 {
            return;
        }
        let numerator = pending_wait_44k.saturating_mul(CHIP_RATE as u64) + *bresenham_remainder;
        let chip_samples = numerator / VGM_RATE;
        *bresenham_remainder = numerator % VGM_RATE;
        *pending_wait_44k = 0;
        if chip_samples != 0 {
            ops.push(Op::Samples(chip_samples as usize));
        }
    }

    while idx < end {
        let command_offset = idx;
        let command = data[idx];
        idx += 1;

        match command {
            0x66 => {
                flush_wait(&mut ops, &mut pending_wait_44k, &mut bresenham_remainder);
                break;
            }
            0x61 => {
                pending_wait_44k += read_u16(idx)? as u64;
                idx += 2;
            }
            0x62 => pending_wait_44k += 735,
            0x63 => pending_wait_44k += 882,
            0x70..=0x7f => pending_wait_44k += (command & 0x0f) as u64 + 1,
            0x80..=0x8f => pending_wait_44k += (command & 0x0f) as u64,
            0x5a | 0x5e => {
                ensure(idx, 2)?;
                flush_wait(&mut ops, &mut pending_wait_44k, &mut bresenham_remainder);
                ops.push(Op::Write(data[idx] as u16, data[idx + 1]));
                idx += 2;
            }
            0x5f => {
                ensure(idx, 2)?;
                flush_wait(&mut ops, &mut pending_wait_44k, &mut bresenham_remainder);
                ops.push(Op::Write(data[idx] as u16 | 0x100, data[idx + 1]));
                idx += 2;
            }
            0x30 | 0x31 | 0x4f | 0x50 => {
                ensure(idx, 1)?;
                idx += 1;
            }
            0x51..=0x5d | 0xa0..=0xbf => {
                ensure(idx, 2)?;
                idx += 2;
            }
            0xc0..=0xdf => {
                ensure(idx, 3)?;
                idx += 3;
            }
            0xe0..=0xff => {
                ensure(idx, 4)?;
                idx += 4;
            }
            0x67 => {
                ensure(idx, 6)?;
                if data[idx] != 0x66 {
                    return Err(format!("malformed VGM data block at 0x{command_offset:x}"));
                }
                let size = read_u32(idx + 2)? as usize;
                idx += 6;
                ensure(idx, size)?;
                idx += size;
            }
            0x68 => {
                ensure(idx, 11)?;
                idx += 11;
            }
            0x90 | 0x91 => {
                ensure(idx, 4)?;
                idx += 4;
            }
            0x92 => {
                ensure(idx, 5)?;
                idx += 5;
            }
            0x93 => {
                ensure(idx, 10)?;
                idx += 10;
            }
            0x94 => {
                ensure(idx, 1)?;
                idx += 1;
            }
            0x95 => {
                ensure(idx, 4)?;
                idx += 4;
            }
            _ => {
                return Err(format!(
                    "unsupported VGM command 0x{command:02x} at 0x{command_offset:x}"
                ));
            }
        }
    }

    flush_wait(&mut ops, &mut pending_wait_44k, &mut bresenham_remainder);
    Ok(ops)
}

fn parse_imf(data: &[u8], imf_rate: u32) -> Result<Vec<Op>, String> {
    if imf_rate == 0 {
        return Err("IMF rate must be greater than zero".to_string());
    }

    let body = if data.len() >= 6 {
        let declared_len = u16::from_le_bytes([data[0], data[1]]) as usize;
        if declared_len == data.len() - 2 && declared_len % 4 == 0 {
            &data[2..]
        } else {
            data
        }
    } else {
        data
    };

    if body.len() % 4 != 0 {
        return Err(format!(
            "IMF body length {} is not a multiple of 4 bytes",
            body.len()
        ));
    }

    let mut ops = Vec::with_capacity((body.len() / 4) * 2);
    let mut bresenham_remainder = 0u64;
    for command in body.chunks_exact(4) {
        ops.push(Op::Write(command[0] as u16, command[1]));

        let delay = u16::from_le_bytes([command[2], command[3]]) as u64;
        if delay == 0 {
            continue;
        }

        let numerator = delay
            .saturating_mul(CHIP_RATE as u64)
            .saturating_add(bresenham_remainder);
        let chip_samples = numerator / imf_rate as u64;
        bresenham_remainder = numerator % imf_rate as u64;
        if chip_samples != 0 {
            ops.push(Op::Samples(chip_samples as usize));
        }
    }

    Ok(ops)
}

fn read_workspace_file(path_env: &str, default_path: &str, kind: &str) -> Vec<u8> {
    let path = std::env::var(path_env).unwrap_or_else(|_| default_path.to_string());
    std::fs::read(&path).unwrap_or_else(|err| {
        panic!("failed to read {kind} file at {path:?}: {err}; set {path_env} to override")
    })
}

fn read_workspace_vgm(path_env: &str, default_path: &str) -> Vec<u8> {
    read_workspace_file(path_env, default_path, "VGM")
}

fn read_workspace_imf(path_env: &str, default_path: &str) -> Vec<u8> {
    read_workspace_file(path_env, default_path, "IMF")
}

fn write_both(
    rust_chip: &mut Opl3Chip,
    c_chip: &mut bindings::Opl3Chip,
    reg: u16,
    value: u8,
    buffered: bool,
) {
    if buffered {
        rust_chip.write_register_buffered(reg, value);
        unsafe {
            bindings::Opl3WriteRegBuffered(c_chip, reg, value);
        }
    } else {
        rust_chip.write_register(reg, value);
        unsafe {
            bindings::Opl3WriteReg(c_chip, reg, value);
        }
    }
}

fn assert_samples_eq(
    rust_samples: &[i16],
    c_samples: &[i16],
    channels: usize,
    start_frame: usize,
    context: &str,
) {
    assert_eq!(
        rust_samples.len(),
        c_samples.len(),
        "{context}: sample buffer length mismatch"
    );
    for (sample_idx, (&rust, &c_ref)) in rust_samples.iter().zip(c_samples).enumerate() {
        assert_eq!(
            rust,
            c_ref,
            "{context}: mismatch at frame {}, channel {}",
            start_frame + sample_idx / channels,
            sample_idx % channels
        );
    }
}

fn reset_c_chip(c_chip: &mut CChipStorage, sample_rate: u32) -> &mut bindings::Opl3Chip {
    unsafe {
        bindings::Opl3Reset(c_chip.as_mut_ptr(), sample_rate);
        &mut *c_chip.as_mut_ptr()
    }
}

fn assert_trace_matches_c(ops: &[Op], buffered: bool) {
    let mut rust_chip = Opl3Chip::new(CHIP_RATE);
    let mut c_storage = CChipStorage::new();
    let c_chip = reset_c_chip(&mut c_storage, CHIP_RATE);

    let mut rust_sample = [0i16; 2];
    let mut c_sample = [0i16; 2];
    let mut frame = 0usize;

    for op in ops {
        match *op {
            Op::Write(reg, value) => write_both(&mut rust_chip, c_chip, reg, value, buffered),
            Op::Samples(count) => {
                for _ in 0..count {
                    rust_chip.generate(&mut rust_sample).unwrap();
                    unsafe {
                        bindings::Opl3Generate(c_chip, c_sample.as_mut_ptr());
                    }
                    assert_samples_eq(&rust_sample, &c_sample, 2, frame, "generate");
                    frame += 1;
                }
            }
        }
    }
}

fn trace_sample_count(ops: &[Op]) -> usize {
    ops.iter()
        .map(|op| match *op {
            Op::Write(_, _) => 0,
            Op::Samples(count) => count,
        })
        .sum()
}

fn checksum_sample(checksum: &mut u64, sample: [i16; 2]) {
    let left = sample[0] as u16 as u64;
    let right = sample[1] as u16 as u64;
    *checksum = checksum
        .wrapping_mul(0x100000001b3)
        .wrapping_add(left | (right << 16));
}

fn render_trace_rust(ops: &[Op], buffered: bool) -> u64 {
    let mut chip = Opl3Chip::new(CHIP_RATE);
    let mut sample = [0i16; 2];
    let mut checksum = 0xcbf29ce484222325;

    for op in ops {
        match *op {
            Op::Write(reg, value) => {
                if buffered {
                    chip.write_register_buffered(reg, value);
                } else {
                    chip.write_register(reg, value);
                }
            }
            Op::Samples(count) => {
                for _ in 0..count {
                    chip.generate(&mut sample).unwrap();
                    checksum_sample(&mut checksum, sample);
                }
            }
        }
    }

    checksum
}

fn render_trace_c(ops: &[Op], buffered: bool) -> u64 {
    let mut c_storage = CChipStorage::new();
    let c_chip = reset_c_chip(&mut c_storage, CHIP_RATE);
    let mut sample = [0i16; 2];
    let mut checksum = 0xcbf29ce484222325;

    for op in ops {
        match *op {
            Op::Write(reg, value) => {
                if buffered {
                    unsafe {
                        bindings::Opl3WriteRegBuffered(c_chip, reg, value);
                    }
                } else {
                    unsafe {
                        bindings::Opl3WriteReg(c_chip, reg, value);
                    }
                }
            }
            Op::Samples(count) => {
                for _ in 0..count {
                    unsafe {
                        bindings::Opl3Generate(c_chip, sample.as_mut_ptr());
                    }
                    checksum_sample(&mut checksum, sample);
                }
            }
        }
    }

    checksum
}

fn benchmark_iterations(default: usize) -> usize {
    std::env::var("OPL3_RS_BENCH_ITERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn time_iterations(mut render: impl FnMut() -> u64, iterations: usize) -> (Duration, u64) {
    let start = Instant::now();
    let mut checksum = 0;
    for _ in 0..iterations {
        checksum ^= black_box(render());
    }
    (start.elapsed(), checksum)
}

fn print_benchmark(name: &str, frames: usize, rust_duration: Duration, c_duration: Duration) {
    let rust_secs = rust_duration.as_secs_f64();
    let c_secs = c_duration.as_secs_f64();
    let frames = frames as f64;
    let rust_ns_frame = rust_secs * 1_000_000_000.0 / frames;
    let c_ns_frame = c_secs * 1_000_000_000.0 / frames;
    let rust_vs_c = c_secs / rust_secs;

    println!(
        "{name}: rust={rust_duration:?} ({rust_ns_frame:.2} ns/frame), \
         c={c_duration:?} ({c_ns_frame:.2} ns/frame), rust_vs_c={rust_vs_c:.2}x"
    );
}

fn benchmark_trace(name: &str, ops: &[Op], buffered: bool, default_iterations: usize) {
    let rust_checksum = render_trace_rust(ops, buffered);
    let c_checksum = render_trace_c(ops, buffered);
    assert_eq!(rust_checksum, c_checksum, "{name}: checksum mismatch");

    let iterations = benchmark_iterations(default_iterations);
    let frames = trace_sample_count(ops) * iterations;
    let (rust_duration, rust_timed_checksum) =
        time_iterations(|| render_trace_rust(ops, buffered), iterations);
    let (c_duration, c_timed_checksum) =
        time_iterations(|| render_trace_c(ops, buffered), iterations);
    assert_eq!(
        rust_timed_checksum, c_timed_checksum,
        "{name}: timed checksum mismatch"
    );

    println!("{name}: iterations={iterations}, frames={frames}, checksum=0x{rust_checksum:016x}");
    print_benchmark(name, frames, rust_duration, c_duration);
}

fn assert_trace_matches_c_resampled(ops: &[Op], sample_rate: u32) {
    let mut rust_chip = Opl3Chip::new(sample_rate);
    let mut c_storage = CChipStorage::new();
    let c_chip = reset_c_chip(&mut c_storage, sample_rate);

    let mut rust_sample = [0i16; 2];
    let mut c_sample = [0i16; 2];
    let mut frame = 0usize;

    for op in ops {
        match *op {
            Op::Write(reg, value) => write_both(&mut rust_chip, c_chip, reg, value, false),
            Op::Samples(count) => {
                for _ in 0..count {
                    rust_chip.generate_resampled(&mut rust_sample).unwrap();
                    unsafe {
                        bindings::Opl3GenerateResampled(c_chip, c_sample.as_mut_ptr());
                    }
                    assert_samples_eq(&rust_sample, &c_sample, 2, frame, "generate_resampled");
                    frame += 1;
                }
            }
        }
    }
}

fn assert_trace_matches_c_stream(ops: &[Op], sample_rate: u32) {
    let mut rust_chip = Opl3Chip::new(sample_rate);
    let mut c_storage = CChipStorage::new();
    let c_chip = reset_c_chip(&mut c_storage, sample_rate);

    let mut frame = 0usize;
    for op in ops {
        match *op {
            Op::Write(reg, value) => write_both(&mut rust_chip, c_chip, reg, value, false),
            Op::Samples(count) => {
                let mut rust_buffer = vec![0i16; count * 2];
                let mut c_buffer = vec![0i16; count * 2];
                rust_chip.generate_stream(&mut rust_buffer).unwrap();
                unsafe {
                    bindings::Opl3GenerateStream(c_chip, c_buffer.as_mut_ptr(), count as u32);
                }
                assert_samples_eq(&rust_buffer, &c_buffer, 2, frame, "generate_stream");
                frame += count;
            }
        }
    }
}

fn assert_trace_matches_c_4ch(ops: &[Op]) {
    let mut rust_chip = Opl3Chip::new(CHIP_RATE);
    let mut c_storage = CChipStorage::new();
    let c_chip = reset_c_chip(&mut c_storage, CHIP_RATE);

    let mut rust_sample = [0i16; 4];
    let mut c_sample = [0i16; 4];
    let mut frame = 0usize;

    for op in ops {
        match *op {
            Op::Write(reg, value) => write_both(&mut rust_chip, c_chip, reg, value, false),
            Op::Samples(count) => {
                for _ in 0..count {
                    rust_chip.generate_4ch(&mut rust_sample).unwrap();
                    unsafe {
                        bindings::Opl3Generate4Ch(c_chip, c_sample.as_mut_ptr());
                    }
                    assert_samples_eq(&rust_sample, &c_sample, 4, frame, "generate_4ch");
                    frame += 1;
                }
            }
        }
    }
}

fn assert_trace_matches_c_4ch_resampled(ops: &[Op], sample_rate: u32) {
    let mut rust_chip = Opl3Chip::new(sample_rate);
    let mut c_storage = CChipStorage::new();
    let c_chip = reset_c_chip(&mut c_storage, sample_rate);

    let mut rust_sample = [0i16; 4];
    let mut c_sample = [0i16; 4];
    let mut frame = 0usize;

    for op in ops {
        match *op {
            Op::Write(reg, value) => write_both(&mut rust_chip, c_chip, reg, value, false),
            Op::Samples(count) => {
                for _ in 0..count {
                    rust_chip.generate_4ch_resampled(&mut rust_sample).unwrap();
                    unsafe {
                        bindings::Opl3Generate4ChResampled(c_chip, c_sample.as_mut_ptr());
                    }
                    assert_samples_eq(&rust_sample, &c_sample, 4, frame, "generate_4ch_resampled");
                    frame += 1;
                }
            }
        }
    }
}

fn assert_trace_matches_c_4ch_stream(ops: &[Op], sample_rate: u32) {
    let mut rust_chip = Opl3Chip::new(sample_rate);
    let mut c_storage = CChipStorage::new();
    let c_chip = reset_c_chip(&mut c_storage, sample_rate);

    let mut frame = 0usize;
    for op in ops {
        match *op {
            Op::Write(reg, value) => write_both(&mut rust_chip, c_chip, reg, value, false),
            Op::Samples(count) => {
                let mut rust_front = vec![0i16; count * 2];
                let mut rust_rear = vec![0i16; count * 2];
                let mut c_front = vec![0i16; count * 2];
                let mut c_rear = vec![0i16; count * 2];
                rust_chip
                    .generate_4ch_stream(&mut rust_front, &mut rust_rear)
                    .unwrap();
                unsafe {
                    bindings::Opl3Generate4ChStream(
                        c_chip,
                        c_front.as_mut_ptr(),
                        c_rear.as_mut_ptr(),
                        count as u32,
                    );
                }
                assert_samples_eq(&rust_front, &c_front, 2, frame, "generate_4ch_stream front");
                assert_samples_eq(&rust_rear, &c_rear, 2, frame, "generate_4ch_stream rear");
                frame += count;
            }
        }
    }
}

fn simple_tone_ops() -> Vec<Op> {
    vec![
        Op::Write(0x105, 0x01),
        Op::Write(0x020, 0x21),
        Op::Write(0x040, 0x10),
        Op::Write(0x060, 0xf0),
        Op::Write(0x080, 0x0f),
        Op::Write(0x023, 0x21),
        Op::Write(0x043, 0x00),
        Op::Write(0x063, 0xf0),
        Op::Write(0x083, 0x0f),
        Op::Write(0x0a0, 0x44),
        Op::Write(0x0c0, 0x31),
        Op::Write(0x0b0, 0x32),
        Op::Samples(4096),
        Op::Write(0x0b0, 0x12),
        Op::Samples(1024),
    ]
}

fn four_channel_tone_ops() -> Vec<Op> {
    vec![
        Op::Write(0x105, 0x01),
        Op::Write(0x020, 0x21),
        Op::Write(0x040, 0x10),
        Op::Write(0x060, 0xf0),
        Op::Write(0x080, 0x0f),
        Op::Write(0x023, 0x21),
        Op::Write(0x043, 0x00),
        Op::Write(0x063, 0xf0),
        Op::Write(0x083, 0x0f),
        Op::Write(0x0a0, 0x44),
        Op::Write(0x0c0, 0xf1),
        Op::Write(0x0b0, 0x32),
        Op::Samples(1024),
        Op::Write(0x0b0, 0x12),
        Op::Samples(256),
    ]
}

#[cfg(feature = "stereo-ext")]
fn stereo_extension_pan_ops() -> Vec<Op> {
    vec![
        Op::Write(0x105, 0x03),
        Op::Write(0x0d0, 0x00),
        Op::Write(0x020, 0x21),
        Op::Write(0x040, 0x10),
        Op::Write(0x060, 0xf0),
        Op::Write(0x080, 0x0f),
        Op::Write(0x023, 0x21),
        Op::Write(0x043, 0x00),
        Op::Write(0x063, 0xf0),
        Op::Write(0x083, 0x0f),
        Op::Write(0x0a0, 0x44),
        Op::Write(0x0c0, 0x31),
        Op::Write(0x0b0, 0x32),
        Op::Samples(1024),
        Op::Write(0x0d0, 0x80),
        Op::Samples(1024),
        Op::Write(0x0d0, 0xff),
        Op::Samples(1024),
        Op::Write(0x0b0, 0x12),
        Op::Samples(256),
    ]
}

#[test]
fn rust_core_matches_c_silence() {
    assert_trace_matches_c(&[Op::Samples(1024)], false);
}

#[test]
fn rust_core_matches_c_simple_tone() {
    assert_trace_matches_c(&simple_tone_ops(), false);
}

#[test]
fn rust_core_matches_c_buffered_simple_tone() {
    assert_trace_matches_c(&simple_tone_ops(), true);
}

#[test]
fn rust_core_matches_c_generate_resampled_api() {
    assert_trace_matches_c_resampled(&simple_tone_ops(), 44_100);
}

#[test]
fn rust_core_matches_c_generate_stream_api() {
    assert_trace_matches_c_stream(&simple_tone_ops(), 44_100);
}

#[test]
fn rust_core_matches_c_generate_4ch_api() {
    assert_trace_matches_c_4ch(&four_channel_tone_ops());
}

#[test]
fn rust_core_matches_c_generate_4ch_resampled_api() {
    assert_trace_matches_c_4ch_resampled(&four_channel_tone_ops(), 44_100);
}

#[test]
fn rust_core_matches_c_generate_4ch_stream_api() {
    assert_trace_matches_c_4ch_stream(&four_channel_tone_ops(), 44_100);
}

#[cfg(feature = "stereo-ext")]
#[test]
fn rust_core_matches_c_stereo_extension_pan_registers() {
    assert_trace_matches_c(&stereo_extension_pan_ops(), false);
}

#[test]
fn rust_core_matches_c_reset_api() {
    let mut rust_chip = Opl3Chip::new(CHIP_RATE);
    let mut c_storage = CChipStorage::new();
    let c_chip = reset_c_chip(&mut c_storage, CHIP_RATE);
    let warmup = simple_tone_ops();

    for op in &warmup {
        match *op {
            Op::Write(reg, value) => write_both(&mut rust_chip, c_chip, reg, value, false),
            Op::Samples(count) => {
                let mut rust_sample = [0i16; 2];
                let mut c_sample = [0i16; 2];
                for _ in 0..count.min(32) {
                    rust_chip.generate(&mut rust_sample).unwrap();
                    unsafe {
                        bindings::Opl3Generate(c_chip, c_sample.as_mut_ptr());
                    }
                }
            }
        }
    }

    rust_chip.reset(44_100);
    unsafe {
        bindings::Opl3Reset(c_chip, 44_100);
    }

    let mut rust_sample = [0i16; 2];
    let mut c_sample = [0i16; 2];
    for frame in 0..64 {
        rust_chip.generate_resampled(&mut rust_sample).unwrap();
        unsafe {
            bindings::Opl3GenerateResampled(c_chip, c_sample.as_mut_ptr());
        }
        assert_samples_eq(
            &rust_sample,
            &c_sample,
            2,
            frame,
            "reset/generate_resampled",
        );
    }
}

#[test]
fn rust_core_matches_c_rhythm_mode() {
    let mut ops = vec![Op::Write(0x105, 0x01)];
    for op_idx in [0x10, 0x11, 0x12, 0x13, 0x14, 0x15] {
        ops.extend([
            Op::Write(0x020 + op_idx, 0x01),
            Op::Write(0x040 + op_idx, 0x10),
            Op::Write(0x060 + op_idx, 0xf0),
            Op::Write(0x080 + op_idx, 0x0f),
        ]);
    }
    for ch_idx in [6, 7, 8] {
        ops.extend([
            Op::Write(0x0a0 + ch_idx, 0x44),
            Op::Write(0x0b0 + ch_idx, 0x12),
            Op::Write(0x0c0 + ch_idx, 0x31),
        ]);
    }
    ops.extend([
        Op::Write(0x0bd, 0x3f),
        Op::Samples(8192),
        Op::Write(0x0bd, 0x20),
        Op::Samples(4096),
    ]);
    assert_trace_matches_c(&ops, false);
}

#[test]
fn rust_core_matches_c_waveforms() {
    for wf in 0..8 {
        let ops = vec![
            Op::Write(0x105, 0x01),
            Op::Write(0x001, 0x20),
            Op::Write(0x020, 0x01),
            Op::Write(0x040, 0x00),
            Op::Write(0x060, 0xf0),
            Op::Write(0x080, 0x0f),
            Op::Write(0x0e0, wf),
            Op::Write(0x023, 0x01),
            Op::Write(0x043, 0x3f),
            Op::Write(0x063, 0xf0),
            Op::Write(0x083, 0x0f),
            Op::Write(0x0e3, wf),
            Op::Write(0x0a0, 0x44),
            Op::Write(0x0c0, 0x31),
            Op::Write(0x040, 0x00),
            Op::Write(0x0b0, 0x32),
            Op::Samples(2048),
            Op::Write(0x0b0, 0x12),
            Op::Samples(512),
        ];
        assert_trace_matches_c(&ops, false);
    }
}

#[test]
fn rust_core_matches_c_buffered_dense_writes() {
    let mut ops = vec![Op::Write(0x105, 0x01)];
    for i in 0..50 {
        ops.push(Op::Write(0x040 + (i % 16), i as u8 & 0x3f));
    }
    ops.push(Op::Samples(256));
    assert_trace_matches_c(&ops, true);
}

#[test]
fn rust_core_matches_c_crystal_oscillator_trace_buffered() {
    let ops = parse_trace(CRYSTAL_OSCILLATOR_TRACE);
    assert_trace_matches_c(&ops, true);
}

#[test]
#[ignore = "full 221s VGM parity test; run with `cargo test --release --features c-reference-tests full_crystal -- --ignored --nocapture`"]
fn rust_core_matches_c_full_crystal_oscillator_vgm_buffered() {
    let data = read_workspace_vgm("OPL3_RS_CRYSTAL_VGM", "../crystal_oscillator.vgm");
    let ops = parse_vgm(&data).expect("failed to parse crystal_oscillator.vgm");
    let frames = trace_sample_count(&ops);
    let writes = ops
        .iter()
        .filter(|op| matches!(op, Op::Write(_, _)))
        .count();
    println!(
        "crystal_oscillator.vgm full: writes={writes}, frames={frames}, duration={:.2}s",
        frames as f64 / CHIP_RATE as f64
    );
    assert_trace_matches_c(&ops, true);
}

#[test]
fn rust_core_matches_c_intermission_imf_trace() {
    let ops = parse_trace(INTERMISSION_IMF_TRACE);
    assert_trace_matches_c(&ops, false);
}

#[test]
#[ignore = "benchmark; run with `cargo test --release --features c-reference-tests bench -- --ignored --nocapture`"]
fn bench_crystal_oscillator_vgm_rust_vs_c() {
    let ops = parse_trace(CRYSTAL_OSCILLATOR_TRACE);
    benchmark_trace("crystal_oscillator.vgm_20k_buffered", &ops, true, 50);
}

#[test]
#[ignore = "benchmark; run with `cargo test --release --features c-reference-tests bench -- --ignored --nocapture`"]
fn bench_intermission_imf_rust_vs_c() {
    let ops = parse_trace(INTERMISSION_IMF_TRACE);
    benchmark_trace("intermission_fuck_yeah.imf_20k", &ops, false, 50);
}

#[test]
#[ignore = "full-file benchmark; run with `cargo test --release --features c-reference-tests full_file -- --ignored --nocapture`"]
fn bench_full_file_crystal_oscillator_vgm_rust_vs_c() {
    let data = read_workspace_vgm("OPL3_RS_CRYSTAL_VGM", "../crystal_oscillator.vgm");
    let ops = parse_vgm(&data).expect("failed to parse crystal_oscillator.vgm");
    benchmark_trace("crystal_oscillator.vgm_full_buffered", &ops, true, 1);
}

#[test]
#[ignore = "full-file benchmark; run with `cargo test --release --features c-reference-tests full_file -- --ignored --nocapture`"]
fn bench_full_file_intermission_imf_rust_vs_c() {
    let data = read_workspace_imf(
        "OPL3_RS_INTERMISSION_IMF",
        "../oply-fork/tests/Intermission Fuck Yeah.imf",
    );
    let ops = parse_imf(&data, DEFAULT_IMF_RATE).expect("failed to parse Intermission IMF");
    benchmark_trace("intermission_fuck_yeah.imf_full", &ops, false, 1);
}

// Deterministic SplitMix64 RNG so fuzz failures are reproducible from
// the seed alone. Avoids pulling `rand` into dev-dependencies.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn next_byte(&mut self) -> u8 {
        (self.next_u64() >> 56) as u8
    }

    fn next_below(&mut self, n: u32) -> u32 {
        ((self.next_u64() >> 32) as u32) % n.max(1)
    }
}

fn random_register_addr(rng: &mut SplitMix64) -> u16 {
    // Bias the distribution toward registers that materially affect
    // synthesis. Roughly: 30% slot ops, 20% channel ops, 5% pan, 10%
    // rhythm, 4% 4-op routing, 4% newm/stereo-ext, 4% NTS, the rest
    // uniform across the OPL3 address space.
    let bucket = rng.next_below(100);
    let bank = u16::from((rng.next_u64() & 1) != 0) << 8;
    match bucket {
        0..=29 => {
            // 0x20/0x40/0x60/0x80/0xe0 + 0..=0x15 (slot index space).
            let kind = rng.next_below(5) as usize;
            let base: u16 = [0x20, 0x40, 0x60, 0x80, 0xe0][kind];
            bank | (base + rng.next_below(0x16) as u16)
        }
        30..=49 => {
            // 0xa0/0xb0/0xc0 + 0..=8 (channel space).
            let kind = rng.next_below(3) as usize;
            let base: u16 = [0xa0, 0xb0, 0xc0][kind];
            bank | (base + rng.next_below(9) as u16)
        }
        50..=54 => bank | (0xd0 + rng.next_below(9) as u16),
        55..=64 => 0x0bd, // rhythm control lives only on bank 0
        65..=68 => 0x104, // 4-op enable (bank 1)
        69..=72 => 0x105, // newm / stereo-ext (bank 1)
        73..=76 => 0x008, // NTS / CSW (bank 0)
        _ => bank | rng.next_below(0x100) as u16,
    }
}

fn generate_random_ops(rng: &mut SplitMix64, count: usize) -> Vec<Op> {
    let mut ops = Vec::with_capacity(count);
    // Always enable OPL3 mode at the top so bank-1 registers and the
    // 4-op routing aren't no-ops for the whole run.
    ops.push(Op::Write(0x105, 0x01));

    for _ in 0..count {
        if rng.next_below(100) < 70 {
            let reg = random_register_addr(rng);
            let val = rng.next_byte();
            ops.push(Op::Write(reg, val));
        } else {
            // Render between 1 and 512 samples per chunk. This keeps
            // the trace dense enough to exercise many envelope
            // transitions without making any single render slow.
            let samples = (rng.next_below(512) + 1) as usize;
            ops.push(Op::Samples(samples));
        }
    }
    ops
}

fn fuzz_seed_from_env_or_clock() -> u64 {
    std::env::var("OPL3_RS_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0xdead_beef_cafe_f00d)
        })
}

fn fuzz_one_run(seed: u64, ops_count: usize, buffered: bool) {
    println!("fuzz_random_trace: seed=0x{seed:016x} ops={ops_count} buffered={buffered}");
    let mut rng = SplitMix64::new(seed);
    let ops = generate_random_ops(&mut rng, ops_count);
    let frames = trace_sample_count(&ops);
    println!(
        "fuzz_random_trace: emitted {} ops ({} frames)",
        ops.len(),
        frames
    );
    assert_trace_matches_c(&ops, buffered);
}

#[test]
#[ignore = "fuzz against C reference; run with `cargo test --release --features c-reference-tests fuzz_random_trace -- --ignored --nocapture`"]
fn fuzz_random_trace_matches_c() {
    let ops_count = std::env::var("OPL3_RS_FUZZ_OPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000);

    let seed = fuzz_seed_from_env_or_clock();

    // Run twice with the same seed but different write modes so a
    // single CI invocation covers both the immediate and the buffered
    // write paths through the chip.
    fuzz_one_run(seed, ops_count, false);
    fuzz_one_run(seed, ops_count, true);
}

fn parse_dro(data: &[u8]) -> Result<Vec<Op>, String> {
    if data.len() < 0x18 || &data[..8] != b"DBRAWOPL" {
        return Err("not a DRO file".to_string());
    }
    // DRO v0.1 stores the version as a packed u32 (0x00010000), with
    // the rest of the header laid out as u32 lengthMs / u32 lengthBytes
    // / hardware/format/compression bytes. v2.0 instead uses two u32
    // version fields followed by an entirely different header. We
    // dispatch on the u32 at 0x08 since that disambiguates them.
    let version_word = u32::from_le_bytes([data[0x08], data[0x09], data[0x0a], data[0x0b]]);
    match version_word {
        0x0001_0000 => parse_dro_v1(data),
        2 => parse_dro_v2(data),
        _ => Err(format!("unsupported DRO version word 0x{version_word:08x}")),
    }
}

fn flush_dro_delay(ops: &mut Vec<Op>, pending_ms: &mut u64, bresenham_remainder: &mut u64) {
    if *pending_ms == 0 {
        return;
    }
    // 49716 chip samples per second, DRO timing in ms → multiply by
    // CHIP_RATE/1000, carrying the remainder Bresenham-style so long
    // delays don't drop a sample over the run.
    let numerator = pending_ms.saturating_mul(CHIP_RATE as u64) + *bresenham_remainder;
    let chip_samples = numerator / 1000;
    *bresenham_remainder = numerator % 1000;
    *pending_ms = 0;
    if chip_samples != 0 {
        ops.push(Op::Samples(chip_samples as usize));
    }
}

fn parse_dro_v1(data: &[u8]) -> Result<Vec<Op>, String> {
    // Layout (24-byte header), as written by DOSBox / read by AdPlug:
    //   0x00 "DBRAWOPL"
    //   0x08 u32 version       (= 0x00010000 for v0.1)
    //   0x0c u32 lengthMs      (informational; we accumulate per-cmd)
    //   0x10 u32 lengthBytes   (data section size in bytes)
    //   0x14 u8  hardware      (0=OPL2, 1=OPL3, 2=DualOPL2 in v0.1;
    //                           note this differs from v2.0)
    //   0x15 u8  format        (0=interleaved, the only sane value)
    //   0x16 u8  compression   (0=none)
    //   0x17 u8  padding
    //   0x18 data
    if data.len() < 0x18 {
        return Err("truncated DRO v0.1 header".to_string());
    }
    let length_bytes =
        u32::from_le_bytes([data[0x10], data[0x11], data[0x12], data[0x13]]) as usize;
    let hardware_type = data[0x14];
    let body_start = 0x18usize;
    let body_end = body_start.saturating_add(length_bytes).min(data.len());

    let _ = hardware_type; // informational; we just pass writes through
    let mut ops = Vec::new();
    let mut idx = body_start;
    let mut bank: u16 = 0;
    let mut pending_ms: u64 = 0;
    let mut bresenham: u64 = 0;

    while idx < body_end {
        let cmd = data[idx];
        idx += 1;
        match cmd {
            0x00 => {
                if idx >= body_end {
                    break;
                }
                pending_ms += data[idx] as u64 + 1;
                idx += 1;
            }
            0x01 => {
                if idx + 1 >= body_end {
                    break;
                }
                let delay = u16::from_le_bytes([data[idx], data[idx + 1]]) as u64 + 1;
                pending_ms += delay;
                idx += 2;
            }
            0x02 => bank = 0,
            0x03 => bank = 0x100,
            0x04 => {
                // Escape: next byte is literal register (matching one of
                // 0x00..=0x04), then the value.
                if idx + 1 >= body_end {
                    break;
                }
                let reg = data[idx] as u16;
                let val = data[idx + 1];
                idx += 2;
                flush_dro_delay(&mut ops, &mut pending_ms, &mut bresenham);
                ops.push(Op::Write(bank | reg, val));
            }
            reg => {
                if idx >= body_end {
                    break;
                }
                let val = data[idx];
                idx += 1;
                flush_dro_delay(&mut ops, &mut pending_ms, &mut bresenham);
                ops.push(Op::Write(bank | reg as u16, val));
            }
        }
    }
    flush_dro_delay(&mut ops, &mut pending_ms, &mut bresenham);
    Ok(ops)
}

fn parse_dro_v2(data: &[u8]) -> Result<Vec<Op>, String> {
    // Layout (matches the lem_xmas_001_jb.dro sample and the AdPlug
    // dro2.cpp loader):
    //   0x00 "DBRAWOPL"
    //   0x08 u32 versionMajor   (=2)
    //   0x0c u32 lengthPairs
    //   0x10 u32 lengthMs       (informational)
    //   0x14 u8  hardware       (0=OPL2, 1=DualOPL2, 2=OPL3 in v2.0)
    //   0x15 u8  format         (0 = interleaved code/value pairs)
    //   0x16 u8  compression    (0 = none)
    //   0x17 u8  shortDelayCode
    //   0x18 u8  longDelayCode
    //   0x19 u8  codemapLength
    //   0x1a..   codemap[codemapLength]
    //   then     data: lengthPairs * (code, value)
    if data.len() < 0x1a {
        return Err("truncated DRO v2 header".to_string());
    }
    let length_pairs =
        u32::from_le_bytes([data[0x0c], data[0x0d], data[0x0e], data[0x0f]]) as usize;
    let _hardware_type = data[0x14];
    let format = data[0x15];
    let compression = data[0x16];
    let short_delay_code = data[0x17];
    let long_delay_code = data[0x18];
    let codemap_length = data[0x19] as usize;

    if format != 0 {
        return Err(format!("DRO v2 unsupported format {format}"));
    }
    if compression != 0 {
        return Err(format!("DRO v2 unsupported compression {compression}"));
    }
    if codemap_length > 0x80 {
        return Err(format!(
            "DRO v2 codemap length {codemap_length} exceeds 0x80"
        ));
    }

    let codemap_start = 0x1a;
    let codemap_end = codemap_start + codemap_length;
    if codemap_end > data.len() {
        return Err("truncated DRO v2 codemap".to_string());
    }
    let codemap = &data[codemap_start..codemap_end];

    let data_start = codemap_end;
    let data_end = data_start
        .checked_add(length_pairs.saturating_mul(2))
        .ok_or_else(|| "DRO v2 length_pairs overflow".to_string())?
        .min(data.len());

    let mut ops = Vec::with_capacity(length_pairs);
    let mut pending_ms: u64 = 0;
    let mut bresenham: u64 = 0;

    let mut idx = data_start;
    while idx + 1 < data_end {
        let code = data[idx];
        let value = data[idx + 1];
        idx += 2;

        if code == short_delay_code {
            pending_ms += value as u64 + 1;
        } else if code == long_delay_code {
            pending_ms += (value as u64 + 1) * 256;
        } else {
            let bank = u16::from((code & 0x80) != 0) << 8;
            let map_idx = (code & 0x7f) as usize;
            if map_idx >= codemap.len() {
                return Err(format!("DRO v2 code 0x{code:02x} out of codemap range"));
            }
            // Pass writes through verbatim. The hardware byte is
            // informational and the OPL2/OPL3/DualOPL2 enum encoding
            // disagrees between v0.1 and v2.0 captures anyway, so we
            // trust the file's bank bit rather than re-deriving it
            // from the hardware field.
            let reg = bank | codemap[map_idx] as u16;
            flush_dro_delay(&mut ops, &mut pending_ms, &mut bresenham);
            ops.push(Op::Write(reg, value));
        }
    }
    flush_dro_delay(&mut ops, &mut pending_ms, &mut bresenham);
    Ok(ops)
}

fn read_workspace_dro_optional() -> Option<Vec<u8>> {
    let path = std::env::var("OPL3_RS_DRO").ok()?;
    match std::fs::read(&path) {
        Ok(data) => Some(data),
        Err(err) => {
            panic!("failed to read DRO file at {path:?}: {err}");
        }
    }
}

#[test]
#[ignore = "DRO parity test; set OPL3_RS_DRO=/path/to/file.dro and run with `cargo test --release --features c-reference-tests dro -- --ignored --nocapture`"]
fn rust_core_matches_c_full_dro() {
    let Some(data) = read_workspace_dro_optional() else {
        eprintln!("OPL3_RS_DRO not set; skipping");
        return;
    };
    let ops = parse_dro(&data).expect("failed to parse DRO");
    let frames = trace_sample_count(&ops);
    let writes = ops
        .iter()
        .filter(|op| matches!(op, Op::Write(_, _)))
        .count();
    println!(
        "dro full: writes={writes}, frames={frames}, duration={:.2}s",
        frames as f64 / CHIP_RATE as f64
    );
    assert_trace_matches_c(&ops, false);
}

#[test]
#[ignore = "DRO benchmark; set OPL3_RS_DRO and run with `cargo test --release --features c-reference-tests bench_full_file_dro -- --ignored --nocapture`"]
fn bench_full_file_dro_rust_vs_c() {
    let Some(data) = read_workspace_dro_optional() else {
        eprintln!("OPL3_RS_DRO not set; skipping");
        return;
    };
    let ops = parse_dro(&data).expect("failed to parse DRO");
    benchmark_trace("dro_full", &ops, false, 1);
}
