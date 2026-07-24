#![no_std]
#![no_main]

use core::cell::UnsafeCell;
use core::panic::PanicInfo;

#[cfg(not(target_os = "none"))]
#[global_allocator]
static ALLOC: slope::memory::GlobalSlabHeap = slope::memory::GlobalSlabHeap::new();

use crest::compositor::pipeline::{CompositorPipeline, TILE_SIZE};
use crest::compositor::Rectangle;
use crest::manifold::{DisplayMode, PixelFormat};
use crest::obsidian::{Fixed, ObsidianShell, SdfInstruction, SdfProgram, SemanticAppNode, Vector3};
use crest::quantum_frame_oracle::{FrameObservation, QuantumFrameOracle};
use crest::quantum_tile_field::QuantumTileField;
use slope::quantum_crest::{QuantumDisplayState, QuantumSystemSnapshot};

const WIDTH: u32 = 160;
const HEIGHT: u32 = 90;
const BYTES_PER_PIXEL: u32 = 4;
const PITCH: u32 = WIDTH * BYTES_PER_PIXEL;
const FRAME_BYTES: usize = (PITCH * HEIGHT) as usize;
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

struct RetainedFrame(UnsafeCell<[u8; FRAME_BYTES]>);

// SAFETY: Crest's bootstrap binary has one execution thread. The buffer is
// borrowed only inside `run_first_light` and is never aliased.
unsafe impl Sync for RetainedFrame {}

static FRAME: RetainedFrame = RetainedFrame(UnsafeCell::new([0; FRAME_BYTES]));

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FirstLightError {
    Mode,
    Program,
    Scene,
    Render,
    EmptyFrame,
    DamagePath,
    DamageOracle,
    FrameOracle,
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    match run_first_light() {
        Ok(report) => publish_report(report),
        Err(error) => publish_failure(error),
    }

    loop {
        if slope::process::yield_now().is_err() {
            core::hint::spin_loop();
        }
    }
}

fn run_first_light() -> Result<FirstLightReport, FirstLightError> {
    let mode = DisplayMode::new(WIDTH, HEIGHT, PITCH, PixelFormat::Argb8888)
        .map_err(|_| FirstLightError::Mode)?;

    let sphere = SdfProgram::new(&[SdfInstruction::Sphere {
        center: Vector3::ZERO,
        radius: Fixed::from_ratio(3, 4).map_err(|_| FirstLightError::Program)?,
    }])
    .map_err(|_| FirstLightError::Program)?;

    let satellite = SdfProgram::new(&[SdfInstruction::Sphere {
        center: Vector3 {
            x: Fixed::from_ratio(2, 5).map_err(|_| FirstLightError::Program)?,
            y: Fixed::from_ratio(-1, 5).map_err(|_| FirstLightError::Program)?,
            z: Fixed::ZERO,
        },
        radius: Fixed::from_ratio(1, 4).map_err(|_| FirstLightError::Program)?,
    }])
    .map_err(|_| FirstLightError::Program)?;

    let mut shell = ObsidianShell::new();
    shell
        .assimilate_app(SemanticAppNode::new(
            1,
            1_000,
            Fixed::ZERO,
            Fixed::ZERO,
            [62, 132, 255, 255],
            sphere,
        ))
        .map_err(|_| FirstLightError::Scene)?;
    shell
        .assimilate_app(SemanticAppNode::new(
            2,
            1_400,
            Fixed::from_ratio(2, 5).map_err(|_| FirstLightError::Scene)?,
            Fixed::from_ratio(-1, 5).map_err(|_| FirstLightError::Scene)?,
            [172, 84, 255, 255],
            satellite,
        ))
        .map_err(|_| FirstLightError::Scene)?;

    let mut pipeline = CompositorPipeline::new(mode);
    let mut tile_field =
        QuantumTileField::new(mode).map_err(|_| FirstLightError::DamageOracle)?;
    tile_field.mark_rectangle(
        Rectangle {
            x: 0,
            y: 0,
            width: WIDTH,
            height: HEIGHT,
        },
        1,
        1,
        true,
    );
    let first_schedule = tile_field
        .compile_schedule(tile_field.total_tiles(), 0, 0x5449_4c45_5f52_4f4f)
        .map_err(|_| FirstLightError::DamageOracle)?;

    let mut oracle =
        QuantumFrameOracle::new(0x4652_414d_455f_4f52)
            .map_err(|_| FirstLightError::FrameOracle)?;
    let mut snapshot = QuantumSystemSnapshot::empty();
    snapshot.sequence = 1;
    snapshot.epoch = 1;
    snapshot.logical_tick = 1;
    snapshot.desktop_session = 1;
    snapshot.desktop_generation = 1;
    snapshot.display = QuantumDisplayState {
        width: WIDTH,
        height: HEIGHT,
        pitch: PITCH,
        format: PixelFormat::Argb8888 as u32,
        refresh_millihertz: 60_000,
        beam_position: 0,
        present_sequence: 0,
        frame_budget_ticks: 1_u64 << 56,
        predicted_render_ticks: 0,
        damage_tiles: first_schedule.scheduled as u32,
        total_tiles: tile_field.total_tiles() as u32,
    };

    let first_begin = slope::time::read_counter();
    let first_plan = oracle
        .plan(snapshot, &first_schedule, first_begin)
        .map_err(|_| FirstLightError::FrameOracle)?;

    // SAFETY: `_start` is the sole owner of this retained buffer.
    let frame = unsafe { &mut *FRAME.0.get() };

    let first_tiles = pipeline
        .render_schedule(
            &shell,
            frame,
            &first_schedule.indices()[..first_plan.tile_budget],
        )
        .map_err(|_| FirstLightError::Render)?;
    let first_end = slope::time::read_counter();
    oracle
        .observe(FrameObservation {
            frame_sequence: first_plan.frame_sequence,
            rendered_tiles: first_tiles.max(1) as usize,
            render_ticks: first_end.saturating_sub(first_begin),
            missed_deadline: first_end >= first_plan.deadline_tick,
            present_tick: first_end,
        })
        .map_err(|_| FirstLightError::FrameOracle)?;
    let expected_tiles = WIDTH.div_ceil(TILE_SIZE) * HEIGHT.div_ceil(TILE_SIZE);
    if first_plan.tile_budget != expected_tiles as usize
        || first_tiles != expected_tiles
    {
        return Err(FirstLightError::EmptyFrame);
    }

    let first_root = frame_root(frame);
    if first_root == 0 || frame.iter().all(|byte| *byte == 0) {
        return Err(FirstLightError::EmptyFrame);
    }

    let partial = Rectangle {
        x: (WIDTH / 4) as i32,
        y: (HEIGHT / 4) as i32,
        width: WIDTH / 2,
        height: HEIGHT / 2,
    };
    pipeline.invalidate_rect(
        partial.x as u32,
        partial.y as u32,
        partial.x as u32 + partial.width,
        partial.y as u32 + partial.height,
    );
    tile_field.complete_prefix(
        &first_schedule,
        first_plan.tile_budget,
        first_end,
    );
    tile_field.mark_rectangle(partial, first_end, 2, false);
    let second_schedule = tile_field
        .compile_schedule(tile_field.total_tiles(), 0, 0x5449_4c45_5f52_4f4f)
        .map_err(|_| FirstLightError::DamageOracle)?;
    snapshot.sequence = 2;
    snapshot.logical_tick = first_end.max(2);
    snapshot.display.damage_tiles = second_schedule.scheduled as u32;

    let second_begin = slope::time::read_counter();
    let second_plan = oracle
        .plan(snapshot, &second_schedule, second_begin)
        .map_err(|_| FirstLightError::FrameOracle)?;
    let second_tiles = pipeline
        .render_schedule(
            &shell,
            frame,
            &second_schedule.indices()[..second_plan.tile_budget],
        )
        .map_err(|_| FirstLightError::Render)?;
    let second_end = slope::time::read_counter();
    oracle
        .observe(FrameObservation {
            frame_sequence: second_plan.frame_sequence,
            rendered_tiles: second_tiles.max(1) as usize,
            render_ticks: second_end.saturating_sub(second_begin),
            missed_deadline: second_end >= second_plan.deadline_tick,
            present_tick: second_end,
        })
        .map_err(|_| FirstLightError::FrameOracle)?;
    tile_field.complete_prefix(
        &second_schedule,
        second_plan.tile_budget,
        second_end,
    );
    if second_tiles == 0 || second_tiles >= first_tiles {
        return Err(FirstLightError::DamagePath);
    }

    let second_root = frame_root(frame);
    let stats = pipeline.stats();

    Ok(FirstLightReport {
        first_root,
        second_root,
        first_tiles,
        second_tiles,
        frame_count: stats.frame_count,
        skipped_tiles: stats.tiles_skipped_total,
        first_plan_root: first_plan.root,
        second_plan_root: second_plan.root,
        first_predicted_ticks: first_plan.predicted_render_ticks,
        second_predicted_ticks: second_plan.predicted_render_ticks,
        conformal_guard_ticks: oracle
            .conformal_guard_ticks()
            .map_err(|_| FirstLightError::FrameOracle)?,
    })
}

fn frame_root(bytes: &[u8]) -> u64 {
    let mut state = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        state ^= u64::from(*byte);
        state = state.wrapping_mul(0x0000_0100_0000_01b3);
    }
    state
}

#[derive(Clone, Copy)]
struct FirstLightReport {
    first_root: u64,
    second_root: u64,
    first_tiles: u32,
    second_tiles: u32,
    frame_count: u64,
    skipped_tiles: u64,
    first_plan_root: u64,
    second_plan_root: u64,
    first_predicted_ticks: u64,
    second_predicted_ticks: u64,
    conformal_guard_ticks: u64,
}

fn publish_report(report: FirstLightReport) {
    let mut line = [0_u8; 320];
    let mut cursor = 0_usize;

    cursor = append(&mut line, cursor, b"[CREST] first-light PASS root0=");
    cursor = append_hex(&mut line, cursor, report.first_root);
    cursor = append(&mut line, cursor, b" root1=");
    cursor = append_hex(&mut line, cursor, report.second_root);
    cursor = append(&mut line, cursor, b" tiles=");
    cursor = append_u64(&mut line, cursor, u64::from(report.first_tiles));
    cursor = append(&mut line, cursor, b"/");
    cursor = append_u64(&mut line, cursor, u64::from(report.second_tiles));
    cursor = append(&mut line, cursor, b" frames=");
    cursor = append_u64(&mut line, cursor, report.frame_count);
    cursor = append(&mut line, cursor, b" skipped=");
    cursor = append_u64(&mut line, cursor, report.skipped_tiles);
    cursor = append(&mut line, cursor, b" plan0=");
    cursor = append_hex(&mut line, cursor, report.first_plan_root);
    cursor = append(&mut line, cursor, b" plan1=");
    cursor = append_hex(&mut line, cursor, report.second_plan_root);
    cursor = append(&mut line, cursor, b" predicted=");
    cursor = append_u64(&mut line, cursor, report.first_predicted_ticks);
    cursor = append(&mut line, cursor, b"/");
    cursor = append_u64(&mut line, cursor, report.second_predicted_ticks);
    cursor = append(&mut line, cursor, b" guard=");
    cursor = append_u64(&mut line, cursor, report.conformal_guard_ticks);
    cursor = append(&mut line, cursor, b"\n");

    let _ = slope::io::write(1, &line[..cursor]);
}

fn publish_failure(error: FirstLightError) {
    let message = match error {
        FirstLightError::Mode => b"[CREST] first-light FAIL mode\n".as_slice(),
        FirstLightError::Program => b"[CREST] first-light FAIL program\n".as_slice(),
        FirstLightError::Scene => b"[CREST] first-light FAIL scene\n".as_slice(),
        FirstLightError::Render => b"[CREST] first-light FAIL render\n".as_slice(),
        FirstLightError::EmptyFrame => b"[CREST] first-light FAIL empty-frame\n".as_slice(),
        FirstLightError::DamagePath => b"[CREST] first-light FAIL damage-path\n".as_slice(),
        FirstLightError::DamageOracle => b"[CREST] first-light FAIL damage-oracle\n".as_slice(),
        FirstLightError::FrameOracle => b"[CREST] first-light FAIL frame-oracle\n".as_slice(),
    };
    let _ = slope::io::write(1, message);
}

fn append(target: &mut [u8], cursor: usize, bytes: &[u8]) -> usize {
    let available = target.len().saturating_sub(cursor);
    let length = bytes.len().min(available);
    target[cursor..cursor + length].copy_from_slice(&bytes[..length]);
    cursor + length
}

fn append_hex(target: &mut [u8], mut cursor: usize, value: u64) -> usize {
    cursor = append(target, cursor, b"0x");
    for shift in (0..16).rev() {
        let nibble = ((value >> (shift * 4)) & 0xf) as usize;
        cursor = append(target, cursor, &HEX_DIGITS[nibble..nibble + 1]);
    }
    cursor
}

fn append_u64(target: &mut [u8], cursor: usize, mut value: u64) -> usize {
    let mut digits = [0_u8; 20];
    let mut length = 0_usize;

    if value == 0 {
        return append(target, cursor, b"0");
    }

    while value != 0 && length < digits.len() {
        digits[length] = b'0' + (value % 10) as u8;
        value /= 10;
        length += 1;
    }

    let mut output = cursor;
    for index in (0..length).rev() {
        output = append(target, output, &digits[index..index + 1]);
    }
    output
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    let _ = slope::io::write(1, b"[CREST] first-light PANIC\n");
    loop {
        let _ = slope::process::request_exit(1);
        let _ = slope::process::yield_now();
    }
}
