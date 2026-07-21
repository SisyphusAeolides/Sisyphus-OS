pub const MAXIMUM_SHARED_WINDOWS: usize = 32;
pub const MAXIMUM_WINDOW_PAGES: u32 = 262_144;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowPermissions(u8);

impl WindowPermissions {
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    const KNOWN: u8 = Self::READ.0 | Self::WRITE.0;

    pub const fn read_write() -> Self {
        Self(Self::READ.0 | Self::WRITE.0)
    }

    pub const fn contains(self, permission: Self) -> bool {
        self.0 & permission.0 == permission.0
    }

    const fn valid(self) -> bool {
        self.0 != 0 && self.0 & !Self::KNOWN == 0
    }
}

/// A request for a revocable shared-memory window.
///
/// Every address is represented as an object handle plus a page number. The
/// registry does not touch hardware page tables; a memory manager must resolve
/// the handles, enforce ownership, and install mappings separately.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SharedWindowRequest {
    pub host_address_space: u64,
    pub peer_address_space: u64,
    pub memory_object: u64,
    pub host_page: u64,
    pub peer_page: u64,
    pub page_count: u32,
    pub permissions: WindowPermissions,
    pub granted_epoch: u64,
    pub expires_epoch: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub struct SharedWindowHandle {
    slot: u16,
    generation: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SharedWindowInfo {
    pub request: SharedWindowRequest,
    pub generation: u32,
}

#[derive(Clone, Copy)]
struct WindowSlot {
    occupied: bool,
    generation: u32,
    request: SharedWindowRequest,
}

impl WindowSlot {
    const EMPTY: Self = Self {
        occupied: false,
        generation: 0,
        request: SharedWindowRequest {
            host_address_space: 0,
            peer_address_space: 0,
            memory_object: 0,
            host_page: 0,
            peer_page: 0,
            page_count: 0,
            permissions: WindowPermissions(0),
            granted_epoch: 0,
            expires_epoch: 0,
        },
    };
}

/// Bounded metadata registry for explicit, revocable sharing leases.
pub struct SharedWindowRegistry {
    slots: [WindowSlot; MAXIMUM_SHARED_WINDOWS],
}

impl SharedWindowRegistry {
    pub const fn new() -> Self {
        Self {
            slots: [WindowSlot::EMPTY; MAXIMUM_SHARED_WINDOWS],
        }
    }

    pub fn grant(
        &mut self,
        request: SharedWindowRequest,
    ) -> Result<SharedWindowHandle, EchidnaError> {
        validate_request(request)?;
        let (index, slot) = self
            .slots
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| !slot.occupied)
            .ok_or(EchidnaError::CapacityExceeded)?;
        slot.generation = next_generation(slot.generation);
        slot.request = request;
        slot.occupied = true;
        Ok(SharedWindowHandle {
            slot: index as u16,
            generation: slot.generation,
        })
    }

    pub fn resolve(
        &self,
        handle: &SharedWindowHandle,
        current_epoch: u64,
    ) -> Result<SharedWindowInfo, EchidnaError> {
        let slot = self.slot(handle)?;
        if current_epoch < slot.request.granted_epoch || current_epoch >= slot.request.expires_epoch
        {
            return Err(EchidnaError::LeaseInactive);
        }
        Ok(SharedWindowInfo {
            request: slot.request,
            generation: slot.generation,
        })
    }

    pub fn revoke(
        &mut self,
        handle: &SharedWindowHandle,
    ) -> Result<SharedWindowInfo, EchidnaError> {
        let index = usize::from(handle.slot);
        let slot = self
            .slots
            .get_mut(index)
            .ok_or(EchidnaError::InvalidHandle)?;
        if !slot.occupied || slot.generation != handle.generation {
            return Err(EchidnaError::InvalidHandle);
        }
        slot.occupied = false;
        Ok(SharedWindowInfo {
            request: slot.request,
            generation: slot.generation,
        })
    }

    fn slot(&self, handle: &SharedWindowHandle) -> Result<&WindowSlot, EchidnaError> {
        self.slots
            .get(usize::from(handle.slot))
            .filter(|slot| slot.occupied && slot.generation == handle.generation)
            .ok_or(EchidnaError::InvalidHandle)
    }
}

impl Default for SharedWindowRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_request(request: SharedWindowRequest) -> Result<(), EchidnaError> {
    if request.host_address_space == 0
        || request.peer_address_space == 0
        || request.host_address_space == request.peer_address_space
        || request.memory_object == 0
        || request.page_count == 0
        || request.page_count > MAXIMUM_WINDOW_PAGES
        || !request.permissions.valid()
        || request.expires_epoch <= request.granted_epoch
    {
        return Err(EchidnaError::InvalidRequest);
    }
    request
        .host_page
        .checked_add(u64::from(request.page_count))
        .and_then(|_| request.peer_page.checked_add(u64::from(request.page_count)))
        .ok_or(EchidnaError::InvalidRequest)?;
    Ok(())
}

const fn next_generation(generation: u32) -> u32 {
    let next = generation.wrapping_add(1);
    if next == 0 { 1 } else { next }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EchidnaError {
    InvalidRequest,
    CapacityExceeded,
    InvalidHandle,
    LeaseInactive,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> SharedWindowRequest {
        SharedWindowRequest {
            host_address_space: 1,
            peer_address_space: 2,
            memory_object: 3,
            host_page: 10,
            peer_page: 20,
            page_count: 2,
            permissions: WindowPermissions::read_write(),
            granted_epoch: 5,
            expires_epoch: 9,
        }
    }

    #[test]
    fn grants_resolves_and_revokes_a_bounded_lease() {
        let mut registry = SharedWindowRegistry::new();
        let handle = registry.grant(request()).unwrap();
        assert_eq!(registry.resolve(&handle, 5).unwrap().request.page_count, 2);
        registry.revoke(&handle).unwrap();
        assert_eq!(
            registry.resolve(&handle, 6),
            Err(EchidnaError::InvalidHandle)
        );
    }

    #[test]
    fn rejects_self_splicing_and_expired_leases() {
        let mut registry = SharedWindowRegistry::new();
        let mut invalid = request();
        invalid.peer_address_space = invalid.host_address_space;
        assert_eq!(registry.grant(invalid), Err(EchidnaError::InvalidRequest));

        let handle = registry.grant(request()).unwrap();
        assert_eq!(
            registry.resolve(&handle, 9),
            Err(EchidnaError::LeaseInactive)
        );
    }
}
