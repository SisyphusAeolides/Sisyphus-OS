use slope::quantum_crest::{
    QuantumSystemSnapshot, SNAPSHOT_FLAG_BLACKLAB_DEGRADED, SNAPSHOT_FLAG_QUARANTINE_ACTIVE,
    SNAPSHOT_FLAG_RECOVERY_PENDING, SNAPSHOT_FLAG_SAFE_MODE,
};

use crate::compositor::pipeline::TILE_SIZE;
use crate::manifold::DisplayMode;
use crate::quantum_tile_field::TileSchedule;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AlienPalette {
    pub void: [u8; 4],
    pub plasma: [u8; 4],
    pub lattice: [u8; 4],
    pub warning: [u8; 4],
    pub recovery: [u8; 4],
}

impl AlienPalette {
    pub const OBSIDIAN_CIVILIZATION: Self = Self {
        void: [3, 5, 12, 255],
        plasma: [48, 255, 196, 255],
        lattice: [91, 87, 255, 255],
        warning: [255, 76, 91, 255],
        recovery: [255, 198, 64, 255],
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuraState {
    pub epoch: u64,
    pub scene_root: u64,
    pub focus_root: u64,
    pub logical_tick: u64,
    pub risk: u16,
    pub phase: u16,
    pub flags: u64,
    pub ledger_root: u64,
    pub plan_root: u64,
}

pub struct QuantumAura {
    palette: AlienPalette,
    state: AuraState,
}

impl QuantumAura {
    pub const fn new(palette: AlienPalette) -> Self {
        Self {
            palette,
            state: AuraState {
                epoch: 1,
                scene_root: 0,
                focus_root: 0,
                logical_tick: 0,
                risk: 0,
                phase: 0,
                flags: 0,
                ledger_root: 0,
                plan_root: 0,
            },
        }
    }

    pub fn synchronize(
        &mut self,
        snapshot: QuantumSystemSnapshot,
        scene_root: u64,
        focus_root: u64,
    ) {
        self.state.epoch = self.state.epoch.wrapping_add(1).max(1);
        self.state.scene_root = scene_root;
        self.state.focus_root = focus_root;
        self.state.logical_tick = snapshot.logical_tick;
        self.state.risk = snapshot.blacklab.risk;
        self.state.phase = phase_from_snapshot(snapshot);
        self.state.flags = snapshot.flags;
        self.state.ledger_root = snapshot.blacklab.ledger_root;
        self.state.plan_root = snapshot.blacklab.plan_root;
    }

    pub const fn state(&self) -> AuraState {
        self.state
    }

    pub fn apply_schedule(&self, buffer: &mut [u8], mode: DisplayMode, schedule: &TileSchedule) {
        let tiles_wide = mode.width.div_ceil(TILE_SIZE);

        for &index in schedule.indices() {
            let tx = u32::from(index) % tiles_wide;
            let ty = u32::from(index) / tiles_wide;
            let x0 = tx * TILE_SIZE;
            let y0 = ty * TILE_SIZE;
            let x1 = (x0 + TILE_SIZE).min(mode.width);
            let y1 = (y0 + TILE_SIZE).min(mode.height);

            for y in y0..y1 {
                for x in x0..x1 {
                    self.apply_pixel(buffer, mode, x, y);
                }
            }
        }
    }

    pub fn sample(&self, x: u32, y: u32, mode: DisplayMode) -> [u8; 4] {
        let wave = interference(
            x,
            y,
            self.state.phase,
            self.state.scene_root,
            self.state.focus_root,
        );
        let lattice = lattice_intensity(x, y, self.state.logical_tick);
        let risk = self.state.risk.min(1000) as u32;

        let mut color = mix_color(self.palette.void, self.palette.plasma, wave);
        color = mix_color(color, self.palette.lattice, lattice / 2);

        if self.state.flags & SNAPSHOT_FLAG_QUARANTINE_ACTIVE != 0 {
            color = mix_color(
                color,
                self.palette.warning,
                (risk * 180 / 1000).min(180) as u8,
            );
        }

        if self.state.flags & SNAPSHOT_FLAG_RECOVERY_PENDING != 0 {
            let pulse = temporal_pulse(self.state.logical_tick, x, y);
            color = mix_color(color, self.palette.recovery, pulse);
        }

        if self.state.flags & SNAPSHOT_FLAG_SAFE_MODE != 0 {
            let grayscale = luminance(color);
            color = [grayscale / 2, grayscale, grayscale, 255];
        }

        if self.state.flags & SNAPSHOT_FLAG_BLACKLAB_DEGRADED != 0 {
            let horizon = mode
                .height
                .saturating_mul(risk)
                .checked_div(1000)
                .unwrap_or(0);
            if y >= mode.height.saturating_sub(horizon) {
                color = mix_color(
                    color,
                    self.palette.warning,
                    ((y - mode.height.saturating_sub(horizon))
                        .saturating_mul(128)
                        .checked_div(horizon.max(1))
                        .unwrap_or(0))
                    .min(128) as u8,
                );
            }
        }

        color
    }

    fn apply_pixel(&self, buffer: &mut [u8], mode: DisplayMode, x: u32, y: u32) {
        if x >= mode.width || y >= mode.height {
            return;
        }

        let offset = (y as usize)
            .saturating_mul(mode.pitch as usize)
            .saturating_add((x as usize).saturating_mul(mode.format.bytes_per_pixel() as usize));
        if offset.saturating_add(4) > buffer.len() {
            return;
        }

        let overlay = self.sample(x, y, mode);
        let base = [
            buffer[offset + 2],
            buffer[offset + 1],
            buffer[offset],
            buffer[offset + 3],
        ];

        let alpha = aura_alpha(x, y, self.state.logical_tick, self.state.risk);
        let composed = mix_color(base, overlay, alpha);

        buffer[offset] = composed[2];
        buffer[offset + 1] = composed[1];
        buffer[offset + 2] = composed[0];
        buffer[offset + 3] = composed[3];
    }
}

impl Default for QuantumAura {
    fn default() -> Self {
        Self::new(AlienPalette::OBSIDIAN_CIVILIZATION)
    }
}

fn phase_from_snapshot(snapshot: QuantumSystemSnapshot) -> u16 {
    let mut state = snapshot.sequence
        ^ snapshot.epoch.rotate_left(7)
        ^ snapshot.blacklab.ledger_root
        ^ snapshot.blacklab.plan_root.rotate_right(11)
        ^ snapshot.gpu.negotiated_features;
    state ^= state >> 32;
    state as u16
}

fn interference(x: u32, y: u32, phase: u16, scene_root: u64, focus_root: u64) -> u8 {
    let radial = integer_sqrt(
        u64::from(x).saturating_mul(u64::from(x)) + u64::from(y).saturating_mul(u64::from(y)),
    );
    let phase_word = radial
        .saturating_mul(13)
        .wrapping_add(u64::from(phase))
        .wrapping_add(scene_root.rotate_left((x & 31) as u32))
        .wrapping_add(focus_root.rotate_right((y & 31) as u32));

    let triangle = (phase_word & 0x1ff) as u16;
    if triangle <= 255 {
        triangle as u8
    } else {
        (511 - triangle) as u8
    }
}

fn lattice_intensity(x: u32, y: u32, tick: u64) -> u8 {
    let diagonal = x.wrapping_add(y).wrapping_add(tick as u32) & 31;
    let anti = x.wrapping_sub(y).wrapping_add((tick >> 2) as u32) & 31;
    let grid = (x & 63).min(y & 63);

    let diagonal_falloff = (diagonal.min(32 - diagonal) * 48).min(255) as u8;
    let anti_falloff = (anti.min(32 - anti) * 48).min(255) as u8;
    let diagonal_glow = 255_u8.saturating_sub(diagonal_falloff);
    let anti_glow = 255_u8.saturating_sub(anti_falloff);
    let grid_glow = if grid <= 1 { 96 } else { 0 };

    diagonal_glow.max(anti_glow).max(grid_glow)
}

fn temporal_pulse(tick: u64, x: u32, y: u32) -> u8 {
    let phase = tick
        .wrapping_mul(17)
        .wrapping_add(u64::from(x))
        .wrapping_add(u64::from(y) * 3)
        & 0x1ff;
    if phase <= 255 {
        phase as u8
    } else {
        (511 - phase) as u8
    }
}

fn aura_alpha(x: u32, y: u32, tick: u64, risk: u16) -> u8 {
    let noise = hash32(x ^ y.rotate_left(11) ^ tick as u32 ^ (tick >> 32) as u32);
    let base = 24_u32 + u32::from(risk.min(1000)) * 48 / 1000;
    base.saturating_add(noise & 15).min(96) as u8
}

fn mix_color(first: [u8; 4], second: [u8; 4], alpha: u8) -> [u8; 4] {
    let inverse = u16::from(u8::MAX - alpha);
    let alpha = u16::from(alpha);

    [
        ((u16::from(first[0]) * inverse + u16::from(second[0]) * alpha) / 255) as u8,
        ((u16::from(first[1]) * inverse + u16::from(second[1]) * alpha) / 255) as u8,
        ((u16::from(first[2]) * inverse + u16::from(second[2]) * alpha) / 255) as u8,
        first[3].max(second[3]),
    ]
}

fn luminance(color: [u8; 4]) -> u8 {
    ((u16::from(color[0]) * 54 + u16::from(color[1]) * 183 + u16::from(color[2]) * 19) >> 8) as u8
}

fn hash32(mut value: u32) -> u32 {
    value ^= value >> 16;
    value = value.wrapping_mul(0x7feb_352d);
    value ^= value >> 15;
    value = value.wrapping_mul(0x846c_a68b);
    value ^ (value >> 16)
}

fn integer_sqrt(value: u64) -> u64 {
    if value < 2 {
        return value;
    }

    let mut estimate = 1_u64 << ((64 - value.leading_zeros() as u64).div_ceil(2));
    loop {
        let next = (estimate + value / estimate) / 2;
        if next >= estimate {
            return estimate;
        }
        estimate = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifold::PixelFormat;

    #[test]
    fn aura_is_deterministic_for_one_snapshot() {
        let mut aura = QuantumAura::default();
        let mut snapshot = QuantumSystemSnapshot::empty();
        snapshot.sequence = 1;
        snapshot.epoch = 1;
        snapshot.logical_tick = 100;
        snapshot.desktop_session = 1;
        snapshot.desktop_generation = 1;
        snapshot.blacklab.risk = 700;
        snapshot.flags = SNAPSHOT_FLAG_BLACKLAB_DEGRADED;
        aura.synchronize(snapshot, 0x1234, 0x5678);

        let mode = DisplayMode::new(1920, 1080, 1920 * 4, PixelFormat::Argb8888).unwrap();
        assert_eq!(aura.sample(100, 200, mode), aura.sample(100, 200, mode));
    }
}
