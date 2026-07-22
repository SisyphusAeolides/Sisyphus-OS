use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use slope::resonance_plane::{ResonancePlaneClient, NexusOpcode, NexusTelemetry};
use slope::scheduler::{
    self, PhaseHint, Priority,
};

const HEAT_CRITICAL: u64 = 850_000;
const HEAT_EXCITED: u64 = 500_000;
const CEREBRAL_PLANE_ADDRESS: usize = 0x600_0000_0000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReactorState {
    Observe,
    Rephase,
    Cooldown,
}

pub struct NexusReactor {
    client: ResonancePlaneClient,
    state: ReactorState,
    local_phase: u16,
    coherence: u16,
    last_generation: u32,
    cooldown_passes: u16,
}

impl NexusReactor {
    pub fn new(resonance_capability: u64) -> Self {
        // SAFETY: The kernel maps the plane at CEREBRAL_PLANE_ADDRESS during process installation.
        let client = unsafe {
            ResonancePlaneClient::from_address(CEREBRAL_PLANE_ADDRESS, resonance_capability)
                .expect("Failed to initialize ResonancePlaneClient")
        };
        Self {
            client,
            state: ReactorState::Observe,
            local_phase: 0,
            coherence: 768,
            last_generation: 0,
            cooldown_passes: 0,
        }
    }

    fn observe(&mut self) {
        let _ = self.client.poll_reply();

        let Some(telemetry) = self.client.telemetry() else {
            self.coherence = self.coherence.saturating_sub(8).max(128);
            return;
        };

        let kernel_phase = telemetry.global_phase as u16 & 1023;
        let drift = wrapped_distance(self.local_phase, kernel_phase);

        self.state = if telemetry.heat >= HEAT_CRITICAL {
            ReactorState::Cooldown
        } else if telemetry.heat >= HEAT_EXCITED || drift > 192 {
            ReactorState::Rephase
        } else {
            ReactorState::Observe
        };

        match self.state {
            ReactorState::Observe => {
                self.local_phase =
                    self.local_phase.wrapping_add(17) & 1023;
                self.coherence =
                    self.coherence.saturating_add(4).min(960);
                self.cooldown_passes = 0;

                if telemetry.generation != self.last_generation {
                    if !self.client.has_pending_command() {
                        let _ = self.client.submit(NexusOpcode::QueryStats, [0, 0, 0, 0]);
                    }
                    self.last_generation = telemetry.generation;
                }
            }

            ReactorState::Rephase => {
                self.local_phase = kernel_phase;
                self.coherence =
                    self.coherence.saturating_sub(16).max(384);
                self.cooldown_passes = 0;

                let mass =
                    0x6000_u16.saturating_add(self.coherence << 4);

                if !self.client.has_pending_command() {
                    let _ = self.client.submit(NexusOpcode::SetPriorityMass, [mass as u64, 0, 0, 0]);
                }
            }

            ReactorState::Cooldown => {
                self.coherence =
                    self.coherence.saturating_sub(32).max(128);

                self.cooldown_passes =
                    self.cooldown_passes.saturating_add(1);

                if !self.client.has_pending_command() {
                    let _ = self.client.submit(NexusOpcode::SetPriorityMass, [0x3000, 0, 0, 0]);
                }
            }
        }
    }
}

impl Future for NexusReactor {
    type Output = ();

    fn poll(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<()> {
        self.observe();

        let priority = match self.state {
            ReactorState::Observe => Priority::Nexus,
            ReactorState::Rephase => Priority::Interactive,
            ReactorState::Cooldown => Priority::Background,
        };

        let _ = scheduler::yield_with_hint(PhaseHint {
            phase_bin: self.local_phase,
            coherence: self.coherence,
            priority,
            flags: match self.state {
                ReactorState::Observe => 0,
                ReactorState::Rephase => 1 << 1,
                ReactorState::Cooldown => 1 << 2,
            },
        });

        context.waker().wake_by_ref();
        Poll::Pending
    }
}

fn wrapped_distance(a: u16, b: u16) -> u16 {
    let direct = a.abs_diff(b);
    direct.min(1024 - direct)
}
