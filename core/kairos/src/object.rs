use crate::sync::SpinLock;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ObjectKind {
    Task,
    AddressSpace,
    Channel,
    Memory,
    Driver,
    Device,
    Policy,
    Fabric,
    Clock,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct Rights(u16);

impl Rights {
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const EXECUTE: Self = Self(1 << 2);
    pub const MAP: Self = Self(1 << 3);
    pub const SEND: Self = Self(1 << 4);
    pub const RECEIVE: Self = Self(1 << 5);
    pub const CONTROL: Self = Self(1 << 6);
    pub const MORPH: Self = Self(1 << 7);
    pub const ALL: Self = Self(u16::MAX);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, requested: Self) -> bool {
        self.0 & requested.0 == requested.0
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct Handle {
    slot: u16,
    generation: u32,
    kind: ObjectKind,
    rights: Rights,
}

impl Handle {
    pub const fn kind(&self) -> ObjectKind {
        self.kind
    }

    pub const fn rights(&self) -> Rights {
        self.rights
    }

    pub const fn generation(&self) -> u32 {
        self.generation
    }
}

#[derive(Clone, Copy)]
struct Slot {
    occupied: bool,
    generation: u32,
    kind: ObjectKind,
    maximum_rights: Rights,
    references: u32,
    payload_handle: u64,
}

impl Slot {
    const EMPTY: Self = Self {
        occupied: false,
        generation: 0,
        kind: ObjectKind::Task,
        maximum_rights: Rights(0),
        references: 0,
        payload_handle: 0,
    };
}

pub struct ObjectTable<const CAPACITY: usize> {
    slots: SpinLock<[Slot; CAPACITY]>,
}

impl<const CAPACITY: usize> ObjectTable<CAPACITY> {
    pub const fn new() -> Self {
        Self {
            slots: SpinLock::new([Slot::EMPTY; CAPACITY]),
        }
    }

    pub fn allocate(
        &self,
        kind: ObjectKind,
        payload_handle: u64,
        rights: Rights,
    ) -> Result<Handle, ObjectError> {
        let mut slots = self.slots.lock();
        let (index, slot) = slots
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| !slot.occupied)
            .ok_or(ObjectError::CapacityExceeded)?;
        let slot_index = u16::try_from(index).map_err(|_| ObjectError::CapacityExceeded)?;
        slot.generation = next_generation(slot.generation);
        slot.occupied = true;
        slot.kind = kind;
        slot.maximum_rights = rights;
        slot.references = 1;
        slot.payload_handle = payload_handle;
        Ok(Handle {
            slot: slot_index,
            generation: slot.generation,
            kind,
            rights,
        })
    }

    pub fn duplicate(&self, handle: &Handle, rights: Rights) -> Result<Handle, ObjectError> {
        if !handle.rights.contains(rights) {
            return Err(ObjectError::RightsEscalation);
        }
        let mut slots = self.slots.lock();
        let slot = validate_slot(&mut slots[..], handle)?;
        if !slot.maximum_rights.contains(rights) {
            return Err(ObjectError::RightsEscalation);
        }
        slot.references = slot
            .references
            .checked_add(1)
            .ok_or(ObjectError::ReferenceOverflow)?;
        Ok(Handle {
            slot: handle.slot,
            generation: handle.generation,
            kind: handle.kind,
            rights,
        })
    }

    pub fn resolve(&self, handle: &Handle, required: Rights) -> Result<ObjectInfo, ObjectError> {
        if !handle.rights.contains(required) {
            return Err(ObjectError::InsufficientRights);
        }
        let mut slots = self.slots.lock();
        let slot = validate_slot(&mut slots[..], handle)?;
        Ok(ObjectInfo {
            kind: slot.kind,
            payload_handle: slot.payload_handle,
            rights: handle.rights,
        })
    }

    pub fn close(&self, handle: Handle) -> Result<bool, ObjectError> {
        let mut slots = self.slots.lock();
        let slot = validate_slot(&mut slots[..], &handle)?;
        slot.references -= 1;
        if slot.references == 0 {
            slot.occupied = false;
            slot.maximum_rights = Rights(0);
            slot.payload_handle = 0;
            return Ok(true);
        }
        Ok(false)
    }
}

impl<const CAPACITY: usize> Default for ObjectTable<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_slot<'slots>(
    slots: &'slots mut [Slot],
    handle: &Handle,
) -> Result<&'slots mut Slot, ObjectError> {
    let slot = slots
        .get_mut(usize::from(handle.slot))
        .ok_or(ObjectError::InvalidHandle)?;
    if !slot.occupied || slot.generation != handle.generation || slot.kind != handle.kind {
        return Err(ObjectError::InvalidHandle);
    }
    Ok(slot)
}

const fn next_generation(generation: u32) -> u32 {
    let next = generation.wrapping_add(1);
    if next == 0 { 1 } else { next }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObjectInfo {
    pub kind: ObjectKind,
    pub payload_handle: u64,
    pub rights: Rights,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectError {
    CapacityExceeded,
    InvalidHandle,
    RightsEscalation,
    InsufficientRights,
    ReferenceOverflow,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attenuates_rights_and_reclaims_after_the_last_handle() {
        let table = ObjectTable::<2>::new();
        let full = table
            .allocate(ObjectKind::Memory, 77, Rights::READ.union(Rights::WRITE))
            .unwrap();
        let read_only = table.duplicate(&full, Rights::READ).unwrap();
        assert_eq!(
            table.resolve(&read_only, Rights::WRITE),
            Err(ObjectError::InsufficientRights)
        );
        assert_eq!(
            table
                .resolve(&read_only, Rights::READ)
                .unwrap()
                .payload_handle,
            77
        );
        assert_eq!(table.close(read_only), Ok(false));
        assert_eq!(table.close(full), Ok(true));
        let reused = table
            .allocate(ObjectKind::Device, 99, Rights::CONTROL)
            .unwrap();
        assert_eq!(reused.generation(), 2);
    }

    #[test]
    fn rejects_rights_escalation() {
        let table = ObjectTable::<1>::new();
        let read_only = table.allocate(ObjectKind::Policy, 1, Rights::READ).unwrap();
        assert_eq!(
            table.duplicate(&read_only, Rights::WRITE),
            Err(ObjectError::RightsEscalation)
        );
    }
}
