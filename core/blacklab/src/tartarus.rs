use crate::pythia::CLASSIFIER_FEATURES;

pub const MAXIMUM_TOMBSTONES: usize = 64;
pub const MAXIMUM_DEFERRED_SAMPLES: usize = 32;
pub const MAXIMUM_TOMBSTONE_PAGES: u32 = 262_144;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultAccess {
    Read,
    Write,
    Execute,
}

impl FaultAccess {
    const fn feature(self) -> i32 {
        match self {
            Self::Read => 1,
            Self::Write => 2,
            Self::Execute => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TombstoneRequest {
    pub memory_object: u64,
    pub first_page: u64,
    pub page_count: u32,
    pub retired_epoch: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub struct TombstoneHandle {
    slot: u16,
    generation: u32,
}

#[derive(Clone, Copy)]
struct TombstoneSlot {
    occupied: bool,
    generation: u32,
    request: TombstoneRequest,
}

impl TombstoneSlot {
    const EMPTY: Self = Self {
        occupied: false,
        generation: 0,
        request: TombstoneRequest {
            memory_object: 0,
            first_page: 0,
            page_count: 0,
            retired_epoch: 0,
        },
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FaultSnapshot {
    pub address_space_handle: u64,
    pub memory_object: u64,
    pub page_number: u64,
    pub walker_id: u32,
    pub realm: u16,
    pub access: FaultAccess,
    pub epoch: u64,
    pub semantic_heat: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeferredLearningSample {
    pub features: [i32; CLASSIFIER_FEATURES],
    pub suspicious: bool,
}

impl DeferredLearningSample {
    const EMPTY: Self = Self {
        features: [0; CLASSIFIER_FEATURES],
        suspicious: false,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultDecision {
    Continue,
    Quarantine {
        walker_id: u32,
        memory_object: u64,
        learning_queued: bool,
    },
}

/// Software tombstones and a bounded deferred-learning queue.
///
/// This type owns metadata only. It does not repurpose architecture PTE bits,
/// terminate execution contexts, or train a model in exception context.
pub struct TartarusVoid {
    tombstones: [TombstoneSlot; MAXIMUM_TOMBSTONES],
    samples: [DeferredLearningSample; MAXIMUM_DEFERRED_SAMPLES],
    sample_head: usize,
    sample_len: usize,
    quarantined_faults: u64,
    semantic_entropy: u64,
    dropped_samples: u64,
}

impl TartarusVoid {
    pub const fn new() -> Self {
        Self {
            tombstones: [TombstoneSlot::EMPTY; MAXIMUM_TOMBSTONES],
            samples: [DeferredLearningSample::EMPTY; MAXIMUM_DEFERRED_SAMPLES],
            sample_head: 0,
            sample_len: 0,
            quarantined_faults: 0,
            semantic_entropy: 0,
            dropped_samples: 0,
        }
    }

    pub fn retire(&mut self, request: TombstoneRequest) -> Result<TombstoneHandle, TartarusError> {
        validate_request(request)?;
        let (index, slot) = self
            .tombstones
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| !slot.occupied)
            .ok_or(TartarusError::CapacityExceeded)?;
        slot.generation = next_generation(slot.generation);
        slot.request = request;
        slot.occupied = true;
        Ok(TombstoneHandle {
            slot: index as u16,
            generation: slot.generation,
        })
    }

    pub fn inspect_fault(&mut self, fault: FaultSnapshot) -> FaultDecision {
        if fault.address_space_handle == 0 || fault.memory_object == 0 {
            return FaultDecision::Continue;
        }
        let matched = self.tombstones.iter().any(|slot| {
            slot.occupied
                && slot.request.memory_object == fault.memory_object
                && fault.epoch >= slot.request.retired_epoch
                && page_in_range(fault.page_number, slot.request)
        });
        if !matched {
            return FaultDecision::Continue;
        }

        self.quarantined_faults = self.quarantined_faults.saturating_add(1);
        let entropy = fault.page_number
            ^ fault.memory_object
            ^ fault.address_space_handle
            ^ fault.epoch
            ^ u64::from(fault.walker_id);
        self.semantic_entropy = self.semantic_entropy.saturating_add(entropy);
        let heat = i32::try_from(fault.semantic_heat.min(127)).unwrap_or(127);
        let sample = DeferredLearningSample {
            features: [
                i32::from(fault.realm),
                fault.access.feature(),
                1,
                heat,
                clamp_u64_to_i32(fault.page_number),
                clamp_u64_to_i32(fault.epoch),
                0,
                0,
            ],
            suspicious: true,
        };
        let learning_queued = self.push_sample(sample);
        FaultDecision::Quarantine {
            walker_id: fault.walker_id,
            memory_object: fault.memory_object,
            learning_queued,
        }
    }

    pub fn take_learning_sample(&mut self) -> Option<DeferredLearningSample> {
        if self.sample_len == 0 {
            return None;
        }
        let sample = self.samples[self.sample_head];
        self.sample_head = (self.sample_head + 1) % MAXIMUM_DEFERRED_SAMPLES;
        self.sample_len -= 1;
        Some(sample)
    }

    pub fn reclaim(
        &mut self,
        handle: &TombstoneHandle,
        quiescent_epoch: u64,
    ) -> Result<(), TartarusError> {
        let slot = self
            .tombstones
            .get_mut(usize::from(handle.slot))
            .ok_or(TartarusError::InvalidHandle)?;
        if !slot.occupied || slot.generation != handle.generation {
            return Err(TartarusError::InvalidHandle);
        }
        if quiescent_epoch <= slot.request.retired_epoch {
            return Err(TartarusError::NotQuiescent);
        }
        slot.occupied = false;
        Ok(())
    }

    pub const fn quarantined_faults(&self) -> u64 {
        self.quarantined_faults
    }

    pub const fn semantic_entropy(&self) -> u64 {
        self.semantic_entropy
    }

    pub const fn dropped_samples(&self) -> u64 {
        self.dropped_samples
    }

    fn push_sample(&mut self, sample: DeferredLearningSample) -> bool {
        if self.sample_len == MAXIMUM_DEFERRED_SAMPLES {
            self.dropped_samples = self.dropped_samples.saturating_add(1);
            return false;
        }
        let tail = (self.sample_head + self.sample_len) % MAXIMUM_DEFERRED_SAMPLES;
        self.samples[tail] = sample;
        self.sample_len += 1;
        true
    }
}

impl Default for TartarusVoid {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_request(request: TombstoneRequest) -> Result<(), TartarusError> {
    if request.memory_object == 0
        || request.page_count == 0
        || request.page_count > MAXIMUM_TOMBSTONE_PAGES
        || request
            .first_page
            .checked_add(u64::from(request.page_count))
            .is_none()
    {
        return Err(TartarusError::InvalidRequest);
    }
    Ok(())
}

fn page_in_range(page: u64, request: TombstoneRequest) -> bool {
    let Some(end) = request
        .first_page
        .checked_add(u64::from(request.page_count))
    else {
        return false;
    };
    page >= request.first_page && page < end
}

const fn next_generation(generation: u32) -> u32 {
    let next = generation.wrapping_add(1);
    if next == 0 { 1 } else { next }
}

fn clamp_u64_to_i32(value: u64) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TartarusError {
    InvalidRequest,
    CapacityExceeded,
    InvalidHandle,
    NotQuiescent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tombstone_faults_quarantine_and_defer_learning() {
        let mut void = TartarusVoid::new();
        let handle = void
            .retire(TombstoneRequest {
                memory_object: 7,
                first_page: 20,
                page_count: 2,
                retired_epoch: 4,
            })
            .unwrap();
        let decision = void.inspect_fault(FaultSnapshot {
            address_space_handle: 9,
            memory_object: 7,
            page_number: 21,
            walker_id: 3,
            realm: 1,
            access: FaultAccess::Write,
            epoch: 5,
            semantic_heat: 80,
        });
        assert_eq!(
            decision,
            FaultDecision::Quarantine {
                walker_id: 3,
                memory_object: 7,
                learning_queued: true,
            }
        );
        assert!(void.take_learning_sample().unwrap().suspicious);
        assert_eq!(void.quarantined_faults(), 1);
        void.reclaim(&handle, 5).unwrap();
    }

    #[test]
    fn unrelated_faults_continue_without_training_data() {
        let mut void = TartarusVoid::new();
        void.retire(TombstoneRequest {
            memory_object: 7,
            first_page: 20,
            page_count: 1,
            retired_epoch: 4,
        })
        .unwrap();
        assert_eq!(
            void.inspect_fault(FaultSnapshot {
                address_space_handle: 9,
                memory_object: 8,
                page_number: 20,
                walker_id: 3,
                realm: 1,
                access: FaultAccess::Read,
                epoch: 5,
                semantic_heat: 80,
            }),
            FaultDecision::Continue
        );
        assert!(void.take_learning_sample().is_none());
    }
}
