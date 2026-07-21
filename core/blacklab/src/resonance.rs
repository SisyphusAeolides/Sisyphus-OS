use crate::dialect::{Bus, PersonalityId};
use crate::timeline::{CausalBarrier, TimelineError};

pub const MAXIMUM_CASCADE_PREDICTIONS: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RealmMode {
    Hollow,
    Shadow,
    Veiled,
    Eclipse,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalkerState {
    Parked,
    Active,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalkerSnapshot {
    pub walker_id: u32,
    pub state: WalkerState,
    pub realm_mode: RealmMode,
    pub address_space_handle: u64,
    pub current_page: u64,
    pub capability_fingerprint: u64,
    pub source_node: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CascadeThresholds {
    pub flux: u64,
    pub semantic_heat: u64,
    pub maximum_lane_skew: u64,
    pub prediction_confidence_percent: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CascadeInput<'walkers> {
    pub counter_sample: u64,
    pub global_flux: u64,
    pub semantic_heat: u64,
    pub logic_weight: i64,
    pub epoch: u64,
    pub source_personality: PersonalityId,
    pub walkers: &'walkers [WalkerSnapshot],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnomalyFact {
    pub subject: u64,
    pub predicate: u32,
    pub object: u64,
    pub confidence_percent: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PredictionCommand {
    pub walker_id: u32,
    pub source_node: u16,
    pub address_space_handle: u64,
    pub page_number: u64,
    pub replay_epoch: u64,
    pub semantic_hash: u64,
    pub confidence_percent: u8,
}

impl PredictionCommand {
    const EMPTY: Self = Self {
        walker_id: 0,
        source_node: 0,
        address_space_handle: 0,
        page_number: 0,
        replay_epoch: 0,
        semantic_hash: 0,
        confidence_percent: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MorphRequest {
    pub source: PersonalityId,
    pub desired_bus: Bus,
}

pub struct CascadePlan {
    pub barrier: Option<CausalBarrier>,
    pub anomaly: Option<AnomalyFact>,
    predictions: [PredictionCommand; MAXIMUM_CASCADE_PREDICTIONS],
    prediction_count: usize,
    pub predictions_truncated: bool,
    pub morph: Option<MorphRequest>,
    pub next_epoch: u64,
}

impl CascadePlan {
    pub fn predictions(&self) -> &[PredictionCommand] {
        &self.predictions[..self.prediction_count]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CascadeError {
    InvalidThresholds,
    Timeline(TimelineError),
    EpochOverflow,
}

/// Produces bounded advisory commands from immutable subsystem snapshots.
///
/// The caller must validate each command against current ownership and policy
/// before execution. This function never mutates memory mappings, drivers,
/// schedulers, or hardware.
pub fn plan_cascade(
    input: CascadeInput<'_>,
    thresholds: CascadeThresholds,
) -> Result<CascadePlan, CascadeError> {
    if thresholds.prediction_confidence_percent > 100 {
        return Err(CascadeError::InvalidThresholds);
    }
    let anomalous =
        input.global_flux > thresholds.flux && input.semantic_heat > thresholds.semantic_heat;
    let barrier = anomalous
        .then(|| CausalBarrier::new(0, 1, input.counter_sample, thresholds.maximum_lane_skew))
        .transpose()
        .map_err(CascadeError::Timeline)?;
    let anomaly = barrier.map(|_| AnomalyFact {
        subject: input.epoch,
        predicate: 1,
        object: input.global_flux,
        confidence_percent: 99,
    });

    let mut predictions = [PredictionCommand::EMPTY; MAXIMUM_CASCADE_PREDICTIONS];
    let mut prediction_count = 0;
    let mut predictions_truncated = false;
    for walker in input.walkers.iter().filter(|walker| {
        walker.state == WalkerState::Active && walker.realm_mode == RealmMode::Eclipse
    }) {
        let Some(page_number) = walker.current_page.checked_add(1) else {
            continue;
        };
        let Some(slot) = predictions.get_mut(prediction_count) else {
            predictions_truncated = true;
            break;
        };
        *slot = PredictionCommand {
            walker_id: walker.walker_id,
            source_node: walker.source_node,
            address_space_handle: walker.address_space_handle,
            page_number,
            replay_epoch: input.epoch,
            semantic_hash: walker.capability_fingerprint ^ input.counter_sample,
            confidence_percent: thresholds.prediction_confidence_percent,
        };
        prediction_count += 1;
    }

    Ok(CascadePlan {
        barrier,
        anomaly,
        predictions,
        prediction_count,
        predictions_truncated,
        morph: (input.logic_weight < 0).then_some(MorphRequest {
            source: input.source_personality,
            desired_bus: Bus::Platform,
        }),
        next_epoch: input
            .epoch
            .checked_add(1)
            .ok_or(CascadeError::EpochOverflow)?,
    })
}

#[cfg(test)]
mod tests {
    use crate::dialect::{Personality, Registry};

    use super::*;

    fn source_personality() -> PersonalityId {
        let mut registry = Registry::new();
        registry
            .add_personality(Personality {
                bus: Bus::Pci,
                class: 1,
                vendor_id: 1,
                device_id: 1,
                register_stride: 4,
                irq_style: 1,
                dma_style: 1,
            })
            .unwrap()
    }

    #[test]
    fn emits_bounded_advice_without_mutating_subsystems() {
        let walkers = [WalkerSnapshot {
            walker_id: 3,
            state: WalkerState::Active,
            realm_mode: RealmMode::Eclipse,
            address_space_handle: 11,
            current_page: 20,
            capability_fingerprint: 0x55,
            source_node: 2,
        }];
        let plan = plan_cascade(
            CascadeInput {
                counter_sample: 1_000,
                global_flux: 6_000,
                semantic_heat: 11_000,
                logic_weight: -1,
                epoch: 7,
                source_personality: source_personality(),
                walkers: &walkers,
            },
            CascadeThresholds {
                flux: 5_000,
                semantic_heat: 10_000,
                maximum_lane_skew: 500,
                prediction_confidence_percent: 85,
            },
        )
        .unwrap();
        assert!(plan.barrier.is_some());
        assert!(plan.anomaly.is_some());
        assert_eq!(plan.predictions()[0].page_number, 21);
        assert_eq!(plan.morph.unwrap().desired_bus, Bus::Platform);
        assert_eq!(plan.next_epoch, 8);
    }
}
