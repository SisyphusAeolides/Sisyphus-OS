#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootProtocol {
    Multiboot2,
    Limine,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IgnitionPhase {
    Entry,
    HandoffValidated,
    MemoryReady,
    TopologyReady,
    SubsystemsReady,
    InterruptsReady,
    UserlandReady,
    Online,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IgnitionSummary {
    pub protocol: BootProtocol,
    pub memory_regions: usize,
    pub managed_frames: usize,
    pub free_frames: usize,
    pub processors: usize,
    pub userland_ready: bool,
}

/// Protocol-neutral ordering guard for the kernel bootstrap path.
///
/// It records readiness evidence but does not initialize hardware itself. This
/// lets the current Multiboot2 entry and a future Limine entry converge on the
/// same checked sequence without installing competing `_start` symbols.
pub struct IgnitionSequence {
    protocol: BootProtocol,
    phase: IgnitionPhase,
    memory_regions: usize,
    managed_frames: usize,
    free_frames: usize,
    processors: usize,
    userland_ready: bool,
}

impl IgnitionSequence {
    pub const fn new(protocol: BootProtocol) -> Self {
        Self {
            protocol,
            phase: IgnitionPhase::Entry,
            memory_regions: 0,
            managed_frames: 0,
            free_frames: 0,
            processors: 0,
            userland_ready: false,
        }
    }

    pub fn validate_handoff(&mut self, memory_regions: usize) -> Result<(), IgnitionError> {
        self.require_phase(IgnitionPhase::Entry)?;
        if memory_regions == 0 {
            return Err(IgnitionError::MissingResource);
        }
        self.memory_regions = memory_regions;
        self.phase = IgnitionPhase::HandoffValidated;
        Ok(())
    }

    pub fn memory_ready(
        &mut self,
        managed_frames: usize,
        free_frames: usize,
    ) -> Result<(), IgnitionError> {
        self.require_phase(IgnitionPhase::HandoffValidated)?;
        if managed_frames == 0 || free_frames > managed_frames {
            return Err(IgnitionError::MissingResource);
        }
        self.managed_frames = managed_frames;
        self.free_frames = free_frames;
        self.phase = IgnitionPhase::MemoryReady;
        Ok(())
    }

    pub fn topology_ready(&mut self, processors: usize) -> Result<(), IgnitionError> {
        self.require_phase(IgnitionPhase::MemoryReady)?;
        if processors == 0 {
            return Err(IgnitionError::MissingResource);
        }
        self.processors = processors;
        self.phase = IgnitionPhase::TopologyReady;
        Ok(())
    }

    pub fn subsystems_ready(&mut self) -> Result<(), IgnitionError> {
        self.advance(IgnitionPhase::TopologyReady, IgnitionPhase::SubsystemsReady)
    }

    pub fn interrupts_ready(&mut self) -> Result<(), IgnitionError> {
        self.advance(
            IgnitionPhase::SubsystemsReady,
            IgnitionPhase::InterruptsReady,
        )
    }

    pub fn userland_ready(&mut self) -> Result<(), IgnitionError> {
        self.advance(IgnitionPhase::InterruptsReady, IgnitionPhase::UserlandReady)?;
        self.userland_ready = true;
        Ok(())
    }

    pub fn online(&mut self) -> Result<IgnitionSummary, IgnitionError> {
        self.advance(IgnitionPhase::UserlandReady, IgnitionPhase::Online)?;
        Ok(IgnitionSummary {
            protocol: self.protocol,
            memory_regions: self.memory_regions,
            managed_frames: self.managed_frames,
            free_frames: self.free_frames,
            processors: self.processors,
            userland_ready: self.userland_ready,
        })
    }

    pub const fn phase(&self) -> IgnitionPhase {
        self.phase
    }

    fn advance(
        &mut self,
        expected: IgnitionPhase,
        next: IgnitionPhase,
    ) -> Result<(), IgnitionError> {
        self.require_phase(expected)?;
        self.phase = next;
        Ok(())
    }

    fn require_phase(&self, expected: IgnitionPhase) -> Result<(), IgnitionError> {
        if self.phase == expected {
            Ok(())
        } else {
            Err(IgnitionError::InvalidTransition {
                expected,
                actual: self.phase,
            })
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IgnitionError {
    MissingResource,
    InvalidTransition {
        expected: IgnitionPhase,
        actual: IgnitionPhase,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_every_boot_phase_in_order() {
        let mut ignition = IgnitionSequence::new(BootProtocol::Multiboot2);
        assert!(matches!(
            ignition.topology_ready(4),
            Err(IgnitionError::InvalidTransition { .. })
        ));
        ignition.validate_handoff(7).unwrap();
        ignition.memory_ready(100, 80).unwrap();
        ignition.topology_ready(4).unwrap();
        ignition.subsystems_ready().unwrap();
        ignition.interrupts_ready().unwrap();
        ignition.userland_ready().unwrap();
        let summary = ignition.online().unwrap();
        assert_eq!(ignition.phase(), IgnitionPhase::Online);
        assert_eq!(summary.processors, 4);
        assert!(summary.userland_ready);
    }

    #[test]
    fn rejects_empty_or_inconsistent_handoff_resources() {
        let mut ignition = IgnitionSequence::new(BootProtocol::Multiboot2);
        assert_eq!(
            ignition.validate_handoff(0),
            Err(IgnitionError::MissingResource)
        );
        ignition.validate_handoff(1).unwrap();
        assert_eq!(
            ignition.memory_ready(10, 11),
            Err(IgnitionError::MissingResource)
        );
    }
}
