pub const MAXIMUM_PERSONALITIES: usize = 64;
pub const MAXIMUM_TRANSFORMS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Bus {
    Pci,
    Acpi,
    DeviceTree,
    Virtio,
    Platform,
    Synthetic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PersonalityId(u16);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Personality {
    pub bus: Bus,
    pub class: u32,
    pub vendor_id: u32,
    pub device_id: u32,
    pub register_stride: u16,
    pub irq_style: u8,
    pub dma_style: u8,
}

impl Personality {
    const EMPTY: Self = Self {
        bus: Bus::Synthetic,
        class: 0,
        vendor_id: 0,
        device_id: 0,
        register_stride: 0,
        irq_style: 0,
        dma_style: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Transform {
    pub from: PersonalityId,
    pub to: PersonalityId,
    pub operation_class: u32,
    pub latency_cost: u32,
    pub semantic_loss: u32,
}

impl Transform {
    const EMPTY: Self = Self {
        from: PersonalityId(0),
        to: PersonalityId(0),
        operation_class: 0,
        latency_cost: 0,
        semantic_loss: 0,
    };
}

pub struct Registry {
    personalities: [Personality; MAXIMUM_PERSONALITIES],
    personality_count: usize,
    transforms: [Transform; MAXIMUM_TRANSFORMS],
    transform_count: usize,
}

impl Registry {
    pub const fn new() -> Self {
        Self {
            personalities: [Personality::EMPTY; MAXIMUM_PERSONALITIES],
            personality_count: 0,
            transforms: [Transform::EMPTY; MAXIMUM_TRANSFORMS],
            transform_count: 0,
        }
    }

    pub fn add_personality(
        &mut self,
        personality: Personality,
    ) -> Result<PersonalityId, DialectError> {
        if personality.register_stride == 0 || !personality.register_stride.is_power_of_two() {
            return Err(DialectError::InvalidPersonality);
        }
        let slot = self
            .personalities
            .get_mut(self.personality_count)
            .ok_or(DialectError::CapacityExceeded)?;
        *slot = personality;
        let id = PersonalityId(self.personality_count as u16);
        self.personality_count += 1;
        Ok(id)
    }

    pub fn add_transform(&mut self, transform: Transform) -> Result<(), DialectError> {
        self.personality(transform.from)?;
        self.personality(transform.to)?;
        let slot = self
            .transforms
            .get_mut(self.transform_count)
            .ok_or(DialectError::CapacityExceeded)?;
        *slot = transform;
        self.transform_count += 1;
        Ok(())
    }

    pub fn best_transform(&self, from: PersonalityId, desired_bus: Bus) -> Option<Transform> {
        self.transforms[..self.transform_count]
            .iter()
            .copied()
            .filter(|transform| transform.from == from)
            .filter(|transform| {
                self.personality(transform.to)
                    .is_ok_and(|personality| personality.bus == desired_bus)
            })
            .min_by_key(|transform| (transform.semantic_loss, transform.latency_cost))
    }

    fn personality(&self, id: PersonalityId) -> Result<&Personality, DialectError> {
        self.personalities
            .get(usize::from(id.0))
            .filter(|_| usize::from(id.0) < self.personality_count)
            .ok_or(DialectError::InvalidPersonality)
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DialectError {
    CapacityExceeded,
    InvalidPersonality,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_the_lowest_loss_then_lowest_latency_transform() {
        let mut registry = Registry::new();
        let pci = registry
            .add_personality(Personality {
                bus: Bus::Pci,
                class: 1,
                vendor_id: 1,
                device_id: 1,
                register_stride: 4,
                irq_style: 1,
                dma_style: 1,
            })
            .unwrap();
        let platform = registry
            .add_personality(Personality {
                bus: Bus::Platform,
                class: 1,
                vendor_id: 1,
                device_id: 1,
                register_stride: 4,
                irq_style: 1,
                dma_style: 1,
            })
            .unwrap();
        for (loss, latency) in [(2, 1), (1, 9), (1, 4)] {
            registry
                .add_transform(Transform {
                    from: pci,
                    to: platform,
                    operation_class: 1,
                    latency_cost: latency,
                    semantic_loss: loss,
                })
                .unwrap();
        }
        let best = registry.best_transform(pci, Bus::Platform).unwrap();
        assert_eq!((best.semantic_loss, best.latency_cost), (1, 4));
    }
}
