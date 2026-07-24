use crate::compositor::Rectangle;
use crate::compositor::pipeline::{CompositorPipeline, MAX_TILES, TILE_SIZE};
use crate::input::{InputEvent, IntegratedEvent};
use crate::manifold::DisplayMode;
use crate::quantum_scene::DirtyRectangle;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TilePhase {
    Dormant = 0,
    Predicted = 1,
    Dirty = 2,
    Critical = 3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TileSignal {
    pub phase: TilePhase,
    pub confidence: u8,
    pub heat: u16,
    pub source_mask: u16,
    pub last_tick: u64,
    pub velocity_x: i16,
    pub velocity_y: i16,
}

impl TileSignal {
    const DORMANT: Self = Self {
        phase: TilePhase::Dormant,
        confidence: 0,
        heat: 0,
        source_mask: 0,
        last_tick: 0,
        velocity_x: 0,
        velocity_y: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TileCandidate {
    index: u16,
    key: u64,
}

impl TileCandidate {
    const EMPTY: Self = Self {
        index: 0,
        key: u64::MAX,
    };
}

#[derive(Debug, Eq, PartialEq)]
pub struct TileSchedule {
    indices: [u16; MAX_TILES],
    pub scheduled: usize,
    pub deferred: usize,
    pub dirty: usize,
    pub predicted: usize,
    pub critical: usize,
    pub root: u64,
}

impl TileSchedule {
    pub const fn empty() -> Self {
        Self {
            indices: [0; MAX_TILES],
            scheduled: 0,
            deferred: 0,
            dirty: 0,
            predicted: 0,
            critical: 0,
            root: 0,
        }
    }

    pub fn indices(&self) -> &[u16] {
        &self.indices[..self.scheduled]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DamageWave {
    pub center_x: i32,
    pub center_y: i32,
    pub radius_pixels: u32,
    pub confidence: u8,
    pub heat: u16,
    pub source_mask: u16,
    pub velocity_x: i16,
    pub velocity_y: i16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuantumDamageError {
    InvalidMode,
    Capacity,
    InvalidBudget,
    Arithmetic,
}

pub struct QuantumTileField {
    mode: DisplayMode,
    tiles_wide: u32,
    tiles_tall: u32,
    total_tiles: usize,
    hilbert_order: u8,
    signals: [TileSignal; MAX_TILES],
    epoch: u64,
}

impl QuantumTileField {
    pub fn new(mode: DisplayMode) -> Result<Self, QuantumDamageError> {
        let tiles_wide = mode.width.div_ceil(TILE_SIZE);
        let tiles_tall = mode.height.div_ceil(TILE_SIZE);
        let total = tiles_wide
            .checked_mul(tiles_tall)
            .ok_or(QuantumDamageError::Arithmetic)? as usize;

        if total == 0 || total > MAX_TILES {
            return Err(QuantumDamageError::Capacity);
        }

        let longest = tiles_wide.max(tiles_tall).next_power_of_two();
        let order = longest.trailing_zeros() as u8;

        Ok(Self {
            mode,
            tiles_wide,
            tiles_tall,
            total_tiles: total,
            hilbert_order: order,
            signals: [TileSignal::DORMANT; MAX_TILES],
            epoch: 1,
        })
    }

    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    pub const fn total_tiles(&self) -> usize {
        self.total_tiles
    }

    pub fn signal(&self, index: usize) -> Option<TileSignal> {
        self.signals
            .get(index)
            .copied()
            .filter(|_| index < self.total_tiles)
    }

    pub fn mark_rectangle(
        &mut self,
        rectangle: Rectangle,
        tick: u64,
        source_mask: u16,
        critical: bool,
    ) {
        let Some((x0, y0, x1, y1)) = clip_rectangle(rectangle, self.mode) else {
            return;
        };

        let tx0 = x0 / TILE_SIZE;
        let ty0 = y0 / TILE_SIZE;
        let tx1 = x1.div_ceil(TILE_SIZE).min(self.tiles_wide);
        let ty1 = y1.div_ceil(TILE_SIZE).min(self.tiles_tall);

        for ty in ty0..ty1 {
            for tx in tx0..tx1 {
                let index = self.index(tx, ty);
                let signal = &mut self.signals[index];
                signal.phase = if critical {
                    TilePhase::Critical
                } else if signal.phase != TilePhase::Critical {
                    TilePhase::Dirty
                } else {
                    signal.phase
                };
                signal.confidence = u8::MAX;
                signal.heat = signal
                    .heat
                    .saturating_add(if critical { 1024 } else { 256 });
                signal.source_mask |= source_mask;
                signal.last_tick = tick;
            }
        }

        self.epoch = self.epoch.wrapping_add(1).max(1);
    }

    pub fn absorb_scene_commit(&mut self, dirty: &[DirtyRectangle], tick: u64, source_mask: u16) {
        for pair in dirty {
            self.mark_rectangle(pair.before, tick, source_mask, false);
            self.mark_rectangle(pair.after, tick, source_mask, false);
        }
    }

    pub fn inject_wave(&mut self, wave: DamageWave, tick: u64) {
        if wave.radius_pixels == 0 || wave.confidence == 0 {
            return;
        }

        let radius_tiles = wave.radius_pixels.div_ceil(TILE_SIZE).max(1) as i32;
        let center_tx = wave.center_x.div_euclid(TILE_SIZE as i32);
        let center_ty = wave.center_y.div_euclid(TILE_SIZE as i32);

        for dy in -radius_tiles..=radius_tiles {
            for dx in -radius_tiles..=radius_tiles {
                let tx = center_tx.saturating_add(dx);
                let ty = center_ty.saturating_add(dy);
                if tx < 0 || ty < 0 || tx >= self.tiles_wide as i32 || ty >= self.tiles_tall as i32
                {
                    continue;
                }

                let distance = dx.unsigned_abs().saturating_add(dy.unsigned_abs());
                if distance > radius_tiles as u32 {
                    continue;
                }

                let attenuation = ((distance * 255) / radius_tiles as u32) as u8;
                let confidence = wave.confidence.saturating_sub(attenuation);
                if confidence == 0 {
                    continue;
                }

                let index = self.index(tx as u32, ty as u32);
                let signal = &mut self.signals[index];

                if signal.phase == TilePhase::Dormant
                    || (signal.phase == TilePhase::Predicted && confidence > signal.confidence)
                {
                    signal.phase = TilePhase::Predicted;
                }
                signal.confidence = signal.confidence.max(confidence);
                signal.heat = signal
                    .heat
                    .saturating_add(wave.heat.saturating_sub(distance as u16));
                signal.source_mask |= wave.source_mask;
                signal.last_tick = tick;
                signal.velocity_x = wave.velocity_x;
                signal.velocity_y = wave.velocity_y;
            }
        }

        self.epoch = self.epoch.wrapping_add(1).max(1);
    }

    pub fn predict_input(
        &mut self,
        observed: IntegratedEvent,
        predicted: Option<InputEvent>,
        tick: u64,
    ) {
        let observed_point =
            pointer_point(observed.raw).or(Some((observed.pointer_x, observed.pointer_y)));
        let predicted_point = predicted.and_then(pointer_point);

        if let Some((x, y)) = observed_point {
            let (velocity_x, velocity_y, radius) = if let Some((px, py)) = predicted_point {
                (
                    clamp_i32_to_i16(px.saturating_sub(x)),
                    clamp_i32_to_i16(py.saturating_sub(y)),
                    96,
                )
            } else {
                (0, 0, 48)
            };

            self.inject_wave(
                DamageWave {
                    center_x: x,
                    center_y: y,
                    radius_pixels: radius,
                    confidence: if predicted_point.is_some() { 208 } else { 128 },
                    heat: 512,
                    source_mask: 1 << 0,
                    velocity_x,
                    velocity_y,
                },
                tick,
            );
        }
    }

    pub fn escalate_blacklab(&mut self, risk: u16, tick: u64) {
        if risk < 240 {
            return;
        }

        let critical = risk >= 680;
        let heat = risk.saturating_mul(4);
        let confidence = (risk / 4).min(u16::from(u8::MAX)) as u8;

        for signal in &mut self.signals[..self.total_tiles] {
            if critical {
                signal.phase = TilePhase::Critical;
            } else if signal.phase == TilePhase::Dormant {
                signal.phase = TilePhase::Predicted;
            }
            signal.confidence = signal.confidence.max(confidence);
            signal.heat = signal.heat.saturating_add(heat);
            signal.source_mask |= 1 << 15;
            signal.last_tick = tick;
        }

        self.epoch = self.epoch.wrapping_add(1).max(1);
    }

    pub fn compile_schedule(
        &self,
        tile_budget: usize,
        beam_position: u32,
        secret: u64,
    ) -> Result<TileSchedule, QuantumDamageError> {
        if tile_budget == 0 || tile_budget > self.total_tiles {
            return Err(QuantumDamageError::InvalidBudget);
        }

        let mut candidates = [TileCandidate::EMPTY; MAX_TILES];
        let mut candidate_count = 0_usize;
        let mut schedule = TileSchedule::empty();

        for index in 0..self.total_tiles {
            let signal = self.signals[index];
            match signal.phase {
                TilePhase::Dormant => continue,
                TilePhase::Predicted => schedule.predicted += 1,
                TilePhase::Dirty => schedule.dirty += 1,
                TilePhase::Critical => schedule.critical += 1,
            }

            let tx = index as u32 % self.tiles_wide;
            let ty = index as u32 / self.tiles_wide;
            let priority = tile_priority(signal, ty, beam_position / TILE_SIZE, self.tiles_tall);
            let hilbert = hilbert_index(tx, ty, self.hilbert_order);
            let key = ((u32::MAX - priority) as u64) << 32 | u64::from(hilbert);
            candidates[candidate_count] = TileCandidate {
                index: index as u16,
                key,
            };
            candidate_count += 1;
        }

        candidates[..candidate_count].sort_unstable_by_key(|candidate| candidate.key);

        schedule.scheduled = candidate_count.min(tile_budget);
        schedule.deferred = candidate_count.saturating_sub(schedule.scheduled);
        for (destination, candidate) in schedule.indices[..schedule.scheduled]
            .iter_mut()
            .zip(candidates[..schedule.scheduled].iter())
        {
            *destination = candidate.index;
        }

        schedule.root = schedule_root(secret, self.epoch, beam_position, &schedule);
        Ok(schedule)
    }

    pub fn apply_schedule(&self, schedule: &TileSchedule, pipeline: &mut CompositorPipeline) {
        for &index in schedule.indices() {
            let tx = u32::from(index) % self.tiles_wide;
            let ty = u32::from(index) / self.tiles_wide;
            pipeline.damage.mark_tile(tx, ty);
        }
    }

    pub fn complete_frame(&mut self, schedule: &TileSchedule, tick: u64) {
        self.complete_indices(schedule.indices(), tick);
    }

    /// Completes only the planner-authorized prefix of a schedule.
    ///
    /// Deferred tiles retain their signal state and can be selected by the next
    /// frame without reconstructing damage from external state.
    pub fn complete_prefix(
        &mut self,
        schedule: &TileSchedule,
        completed: usize,
        tick: u64,
    ) {
        let completed = completed.min(schedule.scheduled);
        self.complete_indices(&schedule.indices()[..completed], tick);
    }

    fn complete_indices(&mut self, indices: &[u16], tick: u64) {
        for &index in indices {
            let Some(signal) = self.signals.get_mut(usize::from(index)) else {
                continue;
            };
            signal.phase = TilePhase::Dormant;
            signal.confidence = 0;
            signal.heat >>= 1;
            signal.source_mask = 0;
            signal.last_tick = tick;
            signal.velocity_x = 0;
            signal.velocity_y = 0;
        }

        for signal in &mut self.signals[..self.total_tiles] {
            if signal.phase == TilePhase::Predicted {
                signal.confidence = signal.confidence.saturating_sub(32);
                signal.heat = signal.heat.saturating_sub(64);
                if signal.confidence == 0 {
                    *signal = TileSignal::DORMANT;
                }
            }
        }

        self.epoch = self.epoch.wrapping_add(1).max(1);
    }

    fn index(&self, tx: u32, ty: u32) -> usize {
        (ty * self.tiles_wide + tx) as usize
    }
}

fn tile_priority(signal: TileSignal, tile_y: u32, beam_tile: u32, tiles_tall: u32) -> u32 {
    let phase = match signal.phase {
        TilePhase::Dormant => 0,
        TilePhase::Predicted => 1,
        TilePhase::Dirty => 2,
        TilePhase::Critical => 3,
    };

    let wrapped_distance = {
        let direct = tile_y.abs_diff(beam_tile.min(tiles_tall.saturating_sub(1)));
        let wrap = tiles_tall.saturating_sub(direct);
        direct.min(wrap)
    };
    let beam_affinity = tiles_tall.saturating_sub(wrapped_distance);

    (phase << 28)
        | (u32::from(signal.confidence) << 20)
        | (u32::from(signal.heat.min(4095)) << 8)
        | beam_affinity.min(255)
}

fn hilbert_index(mut x: u32, mut y: u32, order: u8) -> u32 {
    let mut distance = 0_u32;
    let mut scale = 1_u32 << order.saturating_sub(1);

    while scale != 0 {
        let rx = u32::from(x & scale != 0);
        let ry = u32::from(y & scale != 0);
        distance =
            distance.saturating_add(scale.saturating_mul(scale).saturating_mul((3 * rx) ^ ry));
        rotate_hilbert(scale, &mut x, &mut y, rx, ry);
        scale >>= 1;
    }

    distance
}

fn rotate_hilbert(scale: u32, x: &mut u32, y: &mut u32, rx: u32, ry: u32) {
    if ry == 0 {
        if rx == 1 {
            *x = scale.saturating_sub(1).saturating_sub(*x);
            *y = scale.saturating_sub(1).saturating_sub(*y);
        }
        core::mem::swap(x, y);
    }
}

fn schedule_root(secret: u64, epoch: u64, beam_position: u32, schedule: &TileSchedule) -> u64 {
    let mut state = mix(secret, epoch);
    state = mix(state, u64::from(beam_position));
    state = mix(state, schedule.scheduled as u64);
    state = mix(state, schedule.deferred as u64);
    state = mix(state, schedule.dirty as u64);
    state = mix(state, schedule.predicted as u64);
    state = mix(state, schedule.critical as u64);
    for &index in schedule.indices() {
        state = mix(state, u64::from(index));
    }
    state
}

fn clip_rectangle(rectangle: Rectangle, mode: DisplayMode) -> Option<(u32, u32, u32, u32)> {
    let right = rectangle.x.checked_add(rectangle.width as i32)?;
    let bottom = rectangle.y.checked_add(rectangle.height as i32)?;

    let x0 = rectangle.x.max(0).min(mode.width as i32) as u32;
    let y0 = rectangle.y.max(0).min(mode.height as i32) as u32;
    let x1 = right.max(0).min(mode.width as i32) as u32;
    let y1 = bottom.max(0).min(mode.height as i32) as u32;

    (x0 < x1 && y0 < y1).then_some((x0, y0, x1, y1))
}

fn pointer_point(event: InputEvent) -> Option<(i32, i32)> {
    match event {
        InputEvent::PointerAbs { x, y, .. }
        | InputEvent::TouchDown { x, y, .. }
        | InputEvent::TouchMove { x, y, .. }
        | InputEvent::TouchUp { x, y, .. }
        | InputEvent::StylusDown { x, y, .. }
        | InputEvent::StylusMove { x, y, .. }
        | InputEvent::StylusUp { x, y }
        | InputEvent::PointerDown { x, y, .. }
        | InputEvent::PointerMove { x, y, .. }
        | InputEvent::PointerUp { x, y, .. } => Some((x, y)),
        _ => None,
    }
}

fn clamp_i32_to_i16(value: i32) -> i16 {
    value.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

fn mix(mut state: u64, word: u64) -> u64 {
    state ^= word.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifold::PixelFormat;

    fn mode() -> DisplayMode {
        DisplayMode::new(128, 128, 128 * 4, PixelFormat::Argb8888).unwrap()
    }

    #[test]
    fn critical_tiles_outrank_predicted_tiles() {
        let mut field = QuantumTileField::new(mode()).unwrap();
        field.inject_wave(
            DamageWave {
                center_x: 16,
                center_y: 16,
                radius_pixels: 16,
                confidence: 200,
                heat: 100,
                source_mask: 1,
                velocity_x: 0,
                velocity_y: 0,
            },
            1,
        );
        field.mark_rectangle(
            Rectangle {
                x: 96,
                y: 96,
                width: 16,
                height: 16,
            },
            1,
            2,
            true,
        );

        let schedule = field.compile_schedule(4, 0, 0x1234).unwrap();
        let first = usize::from(schedule.indices()[0]);
        assert_eq!(field.signal(first).unwrap().phase, TilePhase::Critical);
    }

    #[test]
    fn completing_a_frame_collapses_scheduled_tiles() {
        let mut field = QuantumTileField::new(mode()).unwrap();
        field.mark_rectangle(
            Rectangle {
                x: 0,
                y: 0,
                width: 32,
                height: 32,
            },
            1,
            1,
            false,
        );
        let schedule = field.compile_schedule(8, 0, 0x5678).unwrap();
        field.complete_frame(&schedule, 2);
        for &index in schedule.indices() {
            assert_eq!(
                field.signal(usize::from(index)).unwrap().phase,
                TilePhase::Dormant
            );
        }
    }
}
