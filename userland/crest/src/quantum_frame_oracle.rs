use slope::quantum_crest::{
    QuantumSystemSnapshot, SNAPSHOT_FLAG_BLACKLAB_DEGRADED, SNAPSHOT_FLAG_DMA_REVOKED,
    SNAPSHOT_FLAG_FRAME_DEADLINE_AT_RISK, SNAPSHOT_FLAG_QUARANTINE_ACTIVE,
    SNAPSHOT_FLAG_RECOVERY_PENDING, SNAPSHOT_FLAG_SAFE_MODE,
};

use crate::quantum_tile_field::TileSchedule;

pub const LANE_LATENCY: u8 = 1 << 0;
pub const LANE_COHERENCE: u8 = 1 << 1;
pub const LANE_THERMAL: u8 = 1 << 2;

const HISTORY: usize = 16;
const Q16_ONE: u64 = 1 << 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameMode {
    Quiescent,
    Predictive,
    Coherent,
    Emergency,
    Recovery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentPhase {
    Immediate,
    BeforeBeam,
    AfterBeam,
    Hold,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LaneDecision {
    pub lane: u8,
    pub mode: FrameMode,
    pub phase: PresentPhase,
    pub tile_budget: usize,
    pub predicted_ticks: u64,
    pub confidence: u8,
    pub score: u32,
}

impl LaneDecision {
    const ZERO: Self = Self {
        lane: 0,
        mode: FrameMode::Quiescent,
        phase: PresentPhase::Hold,
        tile_budget: 0,
        predicted_ticks: 0,
        confidence: 0,
        score: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuantumFramePlan {
    pub frame_sequence: u64,
    pub snapshot_sequence: u64,
    pub mode: FrameMode,
    pub present_phase: PresentPhase,
    pub tile_budget: usize,
    pub predicted_render_ticks: u64,
    pub deadline_tick: u64,
    pub lane_votes: u8,
    pub confidence: u8,
    pub decisions: [LaneDecision; 3],
    pub root: u64,
}

impl QuantumFramePlan {
    pub fn verify(&self, secret: u64) -> bool {
        self.frame_sequence != 0
            && self.snapshot_sequence != 0
            && self.tile_budget != 0
            && self.lane_votes != 0
            && self.root == plan_root(secret, self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameObservation {
    pub frame_sequence: u64,
    pub rendered_tiles: usize,
    pub render_ticks: u64,
    pub missed_deadline: bool,
    pub present_tick: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameOracleError {
    InvalidSecret,
    InvalidSnapshot,
    InvalidSchedule,
    TimeRegression,
    SequenceRegression,
}

#[derive(Clone, Copy)]
struct RenderSample {
    tiles: u32,
    ticks: u64,
    missed: bool,
}

impl RenderSample {
    const ZERO: Self = Self {
        tiles: 0,
        ticks: 0,
        missed: false,
    };
}

pub struct QuantumFrameOracle {
    secret: u64,
    next_frame_sequence: u64,
    last_snapshot_sequence: u64,
    last_tick: u64,
    ticks_per_tile_q16: u64,
    jitter_q16: u64,
    history: [RenderSample; HISTORY],
    history_count: usize,
    history_cursor: usize,
    missed_deadlines: u64,
}

impl QuantumFrameOracle {
    pub fn new(secret: u64) -> Result<Self, FrameOracleError> {
        if secret == 0 {
            return Err(FrameOracleError::InvalidSecret);
        }

        Ok(Self {
            secret,
            next_frame_sequence: 1,
            last_snapshot_sequence: 0,
            last_tick: 0,
            ticks_per_tile_q16: Q16_ONE,
            jitter_q16: Q16_ONE / 4,
            history: [RenderSample::ZERO; HISTORY],
            history_count: 0,
            history_cursor: 0,
            missed_deadlines: 0,
        })
    }

    pub fn plan(
        &mut self,
        snapshot: QuantumSystemSnapshot,
        schedule: &TileSchedule,
        now_tick: u64,
    ) -> Result<QuantumFramePlan, FrameOracleError> {
        if snapshot.sequence == 0 || snapshot.epoch == 0 || snapshot.display.total_tiles == 0 {
            return Err(FrameOracleError::InvalidSnapshot);
        }
        if schedule.scheduled == 0 || schedule.scheduled > snapshot.display.total_tiles as usize {
            return Err(FrameOracleError::InvalidSchedule);
        }
        if self.last_tick != 0 && now_tick < self.last_tick {
            return Err(FrameOracleError::TimeRegression);
        }
        if snapshot.sequence < self.last_snapshot_sequence {
            return Err(FrameOracleError::SequenceRegression);
        }

        let frame_sequence = self.next_frame_sequence;
        self.next_frame_sequence = self.next_frame_sequence.wrapping_add(1).max(1);
        self.last_snapshot_sequence = snapshot.sequence;
        self.last_tick = now_tick;

        let predicted = self.predict_ticks(schedule.scheduled);
        let budget = snapshot.display.frame_budget_ticks.max(1);
        let deadline = now_tick.saturating_add(budget);

        let latency = self.latency_lane(snapshot, schedule, predicted, budget);
        let coherence = self.coherence_lane(snapshot, schedule, predicted, budget);
        let thermal = self.thermal_lane(snapshot, schedule, predicted, budget);
        let decisions = [latency, coherence, thermal];

        let (mode, mode_votes) = majority_mode(&decisions);
        let (phase, phase_votes) = majority_phase(&decisions);
        let lane_votes = mode_votes | phase_votes;

        let tile_budget = consensus_budget(&decisions, schedule.scheduled);
        let confidence = median_u8([
            decisions[0].confidence,
            decisions[1].confidence,
            decisions[2].confidence,
        ]);

        let mut plan = QuantumFramePlan {
            frame_sequence,
            snapshot_sequence: snapshot.sequence,
            mode,
            present_phase: phase,
            tile_budget,
            predicted_render_ticks: predicted,
            deadline_tick: deadline,
            lane_votes,
            confidence,
            decisions,
            root: 0,
        };
        plan.root = plan_root(self.secret, &plan);
        Ok(plan)
    }

    pub fn observe(&mut self, observation: FrameObservation) -> Result<(), FrameOracleError> {
        if observation.frame_sequence == 0 || observation.rendered_tiles == 0 {
            return Err(FrameOracleError::InvalidSchedule);
        }
        if self.last_tick != 0 && observation.present_tick < self.last_tick {
            return Err(FrameOracleError::TimeRegression);
        }

        let observed_q16 = observation
            .render_ticks
            .saturating_mul(Q16_ONE)
            .checked_div(observation.rendered_tiles as u64)
            .unwrap_or(u64::MAX);

        let residual = observed_q16.abs_diff(self.ticks_per_tile_q16);
        self.ticks_per_tile_q16 = ewma(self.ticks_per_tile_q16, observed_q16, 3);
        self.jitter_q16 = ewma(self.jitter_q16, residual, 3);

        self.history[self.history_cursor] = RenderSample {
            tiles: observation.rendered_tiles.min(u32::MAX as usize) as u32,
            ticks: observation.render_ticks,
            missed: observation.missed_deadline,
        };
        self.history_cursor = (self.history_cursor + 1) % HISTORY;
        self.history_count = self.history_count.saturating_add(1).min(HISTORY);

        if observation.missed_deadline {
            self.missed_deadlines = self.missed_deadlines.saturating_add(1);
            self.jitter_q16 = self.jitter_q16.saturating_add(Q16_ONE / 2);
        }

        self.last_tick = observation.present_tick;
        Ok(())
    }

    pub const fn missed_deadlines(&self) -> u64 {
        self.missed_deadlines
    }

    pub const fn ticks_per_tile_q16(&self) -> u64 {
        self.ticks_per_tile_q16
    }

    fn predict_ticks(&self, tiles: usize) -> u64 {
        let base = self
            .ticks_per_tile_q16
            .saturating_mul(tiles as u64)
            .saturating_add(self.jitter_q16.saturating_mul(2))
            >> 16;

        let history_guard = self.history_guard_ticks();
        base.saturating_add(history_guard).max(1)
    }

    fn history_guard_ticks(&self) -> u64 {
        if self.history_count == 0 {
            return 0;
        }

        let mut normalized = [0_u64; HISTORY];
        let mut count = 0_usize;

        for sample in self.history[..self.history_count].iter().copied() {
            if sample.tiles == 0 {
                continue;
            }
            normalized[count] = sample
                .ticks
                .saturating_mul(1024)
                .checked_div(u64::from(sample.tiles))
                .unwrap_or(u64::MAX);
            if sample.missed {
                normalized[count] = normalized[count].saturating_mul(2);
            }
            count += 1;
        }

        if count == 0 {
            return 0;
        }

        normalized[..count].sort_unstable();
        normalized[(count * 3 / 4).min(count - 1)]
            .saturating_mul(self.history_count as u64)
            .checked_div(4096)
            .unwrap_or(0)
    }

    fn latency_lane(
        &self,
        snapshot: QuantumSystemSnapshot,
        schedule: &TileSchedule,
        predicted: u64,
        budget: u64,
    ) -> LaneDecision {
        let at_risk =
            predicted >= budget || snapshot.flags & SNAPSHOT_FLAG_FRAME_DEADLINE_AT_RISK != 0;
        let emergency =
            snapshot.flags & (SNAPSHOT_FLAG_DMA_REVOKED | SNAPSHOT_FLAG_RECOVERY_PENDING) != 0;

        let mode = if emergency {
            FrameMode::Emergency
        } else if at_risk {
            FrameMode::Predictive
        } else {
            FrameMode::Coherent
        };

        let phase = if emergency {
            PresentPhase::Immediate
        } else if at_risk {
            PresentPhase::BeforeBeam
        } else {
            PresentPhase::AfterBeam
        };

        let tile_budget = if at_risk {
            schedule
                .critical
                .saturating_add(schedule.dirty)
                .max(1)
                .min(schedule.scheduled)
        } else {
            schedule.scheduled
        };

        LaneDecision {
            lane: LANE_LATENCY,
            mode,
            phase,
            tile_budget,
            predicted_ticks: predicted,
            confidence: confidence_from_ratio(predicted, budget),
            score: if at_risk { 900 } else { 600 },
        }
    }

    fn coherence_lane(
        &self,
        snapshot: QuantumSystemSnapshot,
        schedule: &TileSchedule,
        predicted: u64,
        budget: u64,
    ) -> LaneDecision {
        let unsafe_state = snapshot.flags
            & (SNAPSHOT_FLAG_DMA_REVOKED
                | SNAPSHOT_FLAG_RECOVERY_PENDING
                | SNAPSHOT_FLAG_SAFE_MODE)
            != 0;

        let mode = if unsafe_state {
            FrameMode::Recovery
        } else if schedule.deferred == 0 {
            FrameMode::Coherent
        } else {
            FrameMode::Predictive
        };

        let phase = if unsafe_state {
            PresentPhase::Hold
        } else if predicted < budget / 2 {
            PresentPhase::AfterBeam
        } else {
            PresentPhase::BeforeBeam
        };

        LaneDecision {
            lane: LANE_COHERENCE,
            mode,
            phase,
            tile_budget: if unsafe_state {
                schedule.critical.max(1).min(schedule.scheduled)
            } else {
                schedule.scheduled
            },
            predicted_ticks: predicted,
            confidence: if schedule.deferred == 0 { 240 } else { 176 },
            score: if unsafe_state { 1000 } else { 750 },
        }
    }

    fn thermal_lane(
        &self,
        snapshot: QuantumSystemSnapshot,
        schedule: &TileSchedule,
        predicted: u64,
        budget: u64,
    ) -> LaneDecision {
        let risk = snapshot.blacklab.risk;
        let degraded = snapshot.flags
            & (SNAPSHOT_FLAG_BLACKLAB_DEGRADED | SNAPSHOT_FLAG_QUARANTINE_ACTIVE)
            != 0;

        let mode = if risk >= 880 {
            FrameMode::Recovery
        } else if risk >= 680 {
            FrameMode::Emergency
        } else if degraded {
            FrameMode::Predictive
        } else {
            FrameMode::Coherent
        };

        let phase = if risk >= 880 {
            PresentPhase::Hold
        } else if degraded {
            PresentPhase::BeforeBeam
        } else {
            PresentPhase::AfterBeam
        };

        let thermal_scale = 1000_u64.saturating_sub(u64::from(risk)).max(100);
        let tile_budget = schedule
            .scheduled
            .saturating_mul(thermal_scale as usize)
            .checked_div(1000)
            .unwrap_or(1)
            .max(schedule.critical.max(1))
            .min(schedule.scheduled);

        LaneDecision {
            lane: LANE_THERMAL,
            mode,
            phase,
            tile_budget,
            predicted_ticks: predicted
                .saturating_mul(schedule.scheduled as u64)
                .checked_div(tile_budget as u64)
                .unwrap_or(predicted),
            confidence: confidence_from_ratio(u64::from(risk), 1000),
            score: u32::from(risk).saturating_add(if degraded { 256 } else { 0 }),
        }
    }
}

fn majority_mode(decisions: &[LaneDecision; 3]) -> (FrameMode, u8) {
    let veto = decisions.iter().max_by_key(|d| d.score).unwrap();
    if veto.score >= 1000 {
        return (veto.mode, veto.lane);
    }

    for decision in decisions {
        let mut votes = 0_u8;
        let mut mask = 0_u8;
        for candidate in decisions {
            if candidate.mode == decision.mode {
                votes += 1;
                mask |= candidate.lane;
            }
        }
        if votes >= 2 {
            return (decision.mode, mask);
        }
    }

    let winner = decisions
        .iter()
        .copied()
        .max_by_key(|decision| mode_rank(decision.mode))
        .unwrap_or(LaneDecision::ZERO);
    (winner.mode, winner.lane)
}

fn majority_phase(decisions: &[LaneDecision; 3]) -> (PresentPhase, u8) {
    for decision in decisions {
        let mut votes = 0_u8;
        let mut mask = 0_u8;
        for candidate in decisions {
            if candidate.phase == decision.phase {
                votes += 1;
                mask |= candidate.lane;
            }
        }
        if votes >= 2 {
            return (decision.phase, mask);
        }
    }

    let winner = decisions
        .iter()
        .copied()
        .max_by_key(|decision| phase_rank(decision.phase))
        .unwrap_or(LaneDecision::ZERO);
    (winner.phase, winner.lane)
}

fn consensus_budget(decisions: &[LaneDecision; 3], maximum: usize) -> usize {
    let min_budget = decisions.iter().map(|d| d.tile_budget).min().unwrap();
    min_budget.max(1).min(maximum)
}

fn mode_rank(mode: FrameMode) -> u8 {
    match mode {
        FrameMode::Quiescent => 0,
        FrameMode::Coherent => 1,
        FrameMode::Predictive => 2,
        FrameMode::Emergency => 3,
        FrameMode::Recovery => 4,
    }
}

fn phase_rank(phase: PresentPhase) -> u8 {
    match phase {
        PresentPhase::AfterBeam => 0,
        PresentPhase::BeforeBeam => 1,
        PresentPhase::Immediate => 2,
        PresentPhase::Hold => 3,
    }
}

fn confidence_from_ratio(value: u64, reference: u64) -> u8 {
    if reference == 0 {
        return u8::MAX;
    }
    value
        .saturating_mul(255)
        .checked_div(reference)
        .unwrap_or(255)
        .min(255) as u8
}

fn median_u8(mut values: [u8; 3]) -> u8 {
    values.sort_unstable();
    values[1]
}

fn ewma(current: u64, target: u64, shift: u32) -> u64 {
    if target >= current {
        current.saturating_add((target - current) >> shift)
    } else {
        current.saturating_sub((current - target) >> shift)
    }
}

fn plan_root(secret: u64, plan: &QuantumFramePlan) -> u64 {
    let mut state = mix(secret, plan.frame_sequence);
    state = mix(state, plan.snapshot_sequence);
    state = mix(state, mode_rank(plan.mode) as u64);
    state = mix(state, phase_rank(plan.present_phase) as u64);
    state = mix(state, plan.tile_budget as u64);
    state = mix(state, plan.predicted_render_ticks);
    state = mix(state, plan.deadline_tick);
    state = mix(state, u64::from(plan.lane_votes));
    state = mix(state, u64::from(plan.confidence));

    for decision in plan.decisions {
        state = mix(state, u64::from(decision.lane));
        state = mix(state, mode_rank(decision.mode) as u64);
        state = mix(state, phase_rank(decision.phase) as u64);
        state = mix(state, decision.tile_budget as u64);
        state = mix(state, decision.predicted_ticks);
        state = mix(state, u64::from(decision.confidence));
        state = mix(state, u64::from(decision.score));
    }

    state
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
    use crate::quantum_tile_field::TileSchedule;

    fn snapshot(risk: u16, flags: u64) -> QuantumSystemSnapshot {
        let mut snapshot = QuantumSystemSnapshot::empty();
        snapshot.sequence = 1;
        snapshot.epoch = 1;
        snapshot.desktop_session = 1;
        snapshot.desktop_generation = 1;
        snapshot.display.total_tiles = 64;
        snapshot.display.frame_budget_ticks = 1000;
        snapshot.blacklab.risk = risk;
        snapshot.flags = flags;
        snapshot
    }

    fn schedule() -> TileSchedule {
        let mut schedule = TileSchedule::empty();
        schedule.scheduled = 16;
        schedule.dirty = 8;
        schedule.predicted = 8;
        schedule.critical = 2;
        schedule
    }

    #[test]
    fn thermal_quorum_reduces_work_under_high_risk() {
        let mut oracle = QuantumFrameOracle::new(0x1234).unwrap();
        let plan = oracle
            .plan(
                snapshot(900, SNAPSHOT_FLAG_BLACKLAB_DEGRADED),
                &schedule(),
                10,
            )
            .unwrap();

        assert!(matches!(
            plan.mode,
            FrameMode::Emergency | FrameMode::Recovery
        ));
        assert!(plan.tile_budget <= 16);
        assert!(plan.verify(0x1234));
    }

    #[test]
    fn observations_adapt_ticks_per_tile() {
        let mut oracle = QuantumFrameOracle::new(0x5678).unwrap();
        let before = oracle.ticks_per_tile_q16();
        oracle
            .observe(FrameObservation {
                frame_sequence: 1,
                rendered_tiles: 10,
                render_ticks: 100,
                missed_deadline: false,
                present_tick: 100,
            })
            .unwrap();
        assert_ne!(oracle.ticks_per_tile_q16(), before);
    }
}
