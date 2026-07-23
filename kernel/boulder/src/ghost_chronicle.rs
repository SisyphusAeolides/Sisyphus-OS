#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct GhostEvent {
    pub sequence: u64,
    pub tick: u64,
    pub kind: u16,
    pub cpu: u16,
    pub flags: u32,
    pub argument_zero: u64,
    pub argument_one: u64,
}

impl GhostEvent {
    pub const ZERO: Self = Self {
        sequence: 0,
        tick: 0,
        kind: 0,
        cpu: 0,
        flags: 0,
        argument_zero: 0,
        argument_one: 0,
    };
}

#[derive(Clone, Copy)]
struct ChronicleSlot {
    valid: bool,
    before: u64,
    after: u64,
    event: GhostEvent,
}

impl ChronicleSlot {
    const EMPTY: Self = Self {
        valid: false,
        before: 0,
        after: 0,
        event: GhostEvent::ZERO,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GhostCheckpoint {
    pub next_sequence: u64,
    pub digest: u64,
    pub retained: usize,
}

pub struct GhostChronicle<const N: usize> {
    slots: [ChronicleSlot; N],
    write_index: usize,
    retained: usize,
    next_sequence: u64,
    digest: u64,
}

impl<const N: usize> GhostChronicle<N> {
    pub const fn new(seed: u64) -> Self {
        Self {
            slots: [ChronicleSlot::EMPTY; N],
            write_index: 0,
            retained: 0,
            next_sequence: 1,
            digest: seed,
        }
    }

    pub fn record(
        &mut self,
        tick: u64,
        cpu: u16,
        kind: u16,
        flags: u32,
        argument_zero: u64,
        argument_one: u64,
    ) -> Option<u64> {
        if N == 0 {
            return None;
        }

        let event = GhostEvent {
            sequence: self.next_sequence,
            tick,
            kind,
            cpu,
            flags,
            argument_zero,
            argument_one,
        };

        self.next_sequence = self.next_sequence.wrapping_add(1).max(1);

        let before = self.digest;
        let after = fold_event(before, event);

        self.slots[self.write_index] = ChronicleSlot {
            valid: true,
            before,
            after,
            event,
        };

        self.write_index = (self.write_index + 1) % N;
        self.retained = (self.retained + 1).min(N);
        self.digest = after;

        Some(event.sequence)
    }

    pub const fn checkpoint(&self) -> GhostCheckpoint {
        GhostCheckpoint {
            next_sequence: self.next_sequence,
            digest: self.digest,
            retained: self.retained,
        }
    }

    pub fn verify(&self) -> bool {
        if self.retained == 0 {
            return true;
        }

        let start = (self.write_index + N - self.retained) % N;

        let first = self.slots[start];
        if !first.valid {
            return false;
        }

        let mut expected_before = first.before;

        for offset in 0..self.retained {
            let slot = self.slots[(start + offset) % N];

            if !slot.valid || slot.before != expected_before {
                return false;
            }

            let expected_after = fold_event(slot.before, slot.event);

            if slot.after != expected_after {
                return false;
            }

            expected_before = slot.after;
        }

        expected_before == self.digest
    }

    pub fn latest(&self) -> Option<GhostEvent> {
        if self.retained == 0 {
            return None;
        }

        let index = (self.write_index + N - 1) % N;
        self.slots[index].valid.then_some(self.slots[index].event)
    }

    pub const fn retained(&self) -> usize {
        self.retained
    }
}

fn fold_event(mut digest: u64, event: GhostEvent) -> u64 {
    digest = mix(digest, event.sequence);
    digest = mix(digest, event.tick);
    digest = mix(digest, u64::from(event.kind));
    digest = mix(digest, u64::from(event.cpu));
    digest = mix(digest, u64::from(event.flags));
    digest = mix(digest, event.argument_zero);
    mix(digest, event.argument_one)
}

fn mix(mut state: u64, value: u64) -> u64 {
    state ^= value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    state = state.rotate_left(27);
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}

pub mod ghost_kind {
    pub const NEXUS_TICK: u16 = 1;
    pub const ENTANGLE: u16 = 2;
    pub const COLLAPSE: u16 = 3;
    pub const QUARANTINE: u16 = 4;
    pub const KAIROS_BOOST: u16 = 5;
    pub const THERMAL_LIMIT: u16 = 6;
    pub const PARADOX_COMMIT: u16 = 7;
    pub const PARADOX_ROLLBACK: u16 = 8;
    pub const WORLD_SELECTED: u16 = 9;
    pub const CAUSAL_REJECTION: u16 = 10;
    // Manifold orchestrator
    pub const MANIFOLD_BOOT: u16 = 0xA001;
    pub const HODGE_HEAT: u16 = 0xA002;
    pub const CLUSTER_MUT: u16 = 0xA003;
    pub const NTT_PICK: u16 = 0xA004;
    pub const COMPLEX_ID: u16 = 0xA005;
    pub const SEED_REPORT: u16 = 0xA006;
    pub const ZX_REWRITE: u16 = 0xA010;
    pub const FIEDLER_CUT: u16 = 0xA011;
    pub const CECH_H1: u16 = 0xA012;
    pub const TROPICAL_CRIT: u16 = 0xA013;
}
