use blacklab::oureboros::ArtifactMeasurement;

use crate::capability::{Capability, ProcessInstallControl};
use crate::module::loader::LoaderError;
use crate::process::image::PreparedUserImage;

pub const MAXIMUM_PROCESS_SEGMENTS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MappingPermissions {
    pub readable: bool,
    pub writable: bool,
    pub executable: bool,
}

/// Backend contract for a transactional user-image installation.
///
/// `map_zeroed` must create inaccessible staging memory. `seal` publishes the
/// final user permissions, and `commit` may make the address space schedulable
/// only after every mapping has been verified and sealed. On any intermediate
/// error the installer invokes `abort`.
pub trait UserAddressSpaceBackend {
    type Error;
    type Space: Copy;
    type Mapping: Copy;
    type Process;

    fn begin(&mut self, image_start: u64, image_end: u64) -> Result<Self::Space, Self::Error>;

    fn map_zeroed(
        &mut self,
        space: Self::Space,
        virtual_address: u64,
        memory_size: usize,
    ) -> Result<Self::Mapping, Self::Error>;

    fn copy_into(
        &mut self,
        mapping: Self::Mapping,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), Self::Error>;

    fn verify_contents(
        &mut self,
        mapping: Self::Mapping,
        initialized: &[u8],
        memory_size: usize,
    ) -> Result<bool, Self::Error>;

    fn seal(
        &mut self,
        mapping: Self::Mapping,
        permissions: MappingPermissions,
    ) -> Result<(), Self::Error>;

    fn commit(
        &mut self,
        space: Self::Space,
        entry_point: u64,
    ) -> Result<Self::Process, Self::Error>;

    fn abort(&mut self, space: Self::Space) -> Result<(), Self::Error>;

    fn process_info(&self, process: &Self::Process) -> Option<ProcessImageInfo>;

    fn process_generation(&self, process: &Self::Process) -> Option<u32>;

    /// Proves that the committed address space can become the active hardware
    /// translation root while preserving kernel execution.
    ///
    /// # Safety
    ///
    /// The implementation may install process-owned translation state. The
    /// caller must invoke this only during a serialized kernel phase in which
    /// no scheduler or interrupt path can retain the temporary process state.
    unsafe fn validate_activation(
        &mut self,
        process: &Self::Process,
        authority: &Capability<'_, ProcessInstallControl>,
    ) -> Result<(), Self::Error>;

    fn release_process(&mut self, process: &Self::Process) -> Result<(), Self::Error>;
}

#[derive(Debug, Eq, PartialEq)]
pub struct InstalledUserImage<Process> {
    pub process: Process,
    pub entry_point: u64,
    pub segment_count: usize,
    pub measurement: ArtifactMeasurement,
}

pub fn install_user_image<Backend: UserAddressSpaceBackend>(
    image: PreparedUserImage<'_>,
    backend: &mut Backend,
    _authority: &Capability<'_, ProcessInstallControl>,
) -> Result<InstalledUserImage<Backend::Process>, InstallError<Backend::Error>> {
    let plan = *image.plan();
    let space = backend
        .begin(plan.image_start, plan.image_end)
        .map_err(InstallError::Backend)?;

    for segment in plan.segments() {
        let memory_size = match usize::try_from(segment.memory_size) {
            Ok(size) => size,
            Err(_) => {
                return fail_after_abort(backend, space, InstallError::InvalidSegmentSize);
            }
        };
        let mapping = match backend.map_zeroed(space, segment.virtual_address, memory_size) {
            Ok(mapping) => mapping,
            Err(error) => {
                return fail_after_abort(backend, space, InstallError::Backend(error));
            }
        };
        let data = match plan.segment_data(image.bytes(), *segment) {
            Ok(data) => data,
            Err(error) => {
                return fail_after_abort(backend, space, InstallError::Loader(error));
            }
        };
        if let Err(error) = backend.copy_into(mapping, 0, data) {
            return fail_after_abort(backend, space, InstallError::Backend(error));
        }
        match backend.verify_contents(mapping, data, memory_size) {
            Ok(true) => {}
            Ok(false) => {
                return fail_after_abort(backend, space, InstallError::VerificationFailed);
            }
            Err(error) => {
                return fail_after_abort(backend, space, InstallError::Backend(error));
            }
        }
        let permissions = MappingPermissions {
            readable: segment.readable,
            writable: segment.writable,
            executable: segment.executable,
        };
        if permissions.writable && permissions.executable {
            return fail_after_abort(backend, space, InstallError::WriteExecuteMapping);
        }
        if let Err(error) = backend.seal(mapping, permissions) {
            return fail_after_abort(backend, space, InstallError::Backend(error));
        }
    }

    let process = match backend.commit(space, plan.entry_point) {
        Ok(process) => process,
        Err(error) => {
            return fail_after_abort(backend, space, InstallError::Backend(error));
        }
    };
    Ok(InstalledUserImage {
        process,
        entry_point: plan.entry_point,
        segment_count: plan.segments().len(),
        measurement: image.measurement(),
    })
}

fn fail_after_abort<Backend: UserAddressSpaceBackend, T>(
    backend: &mut Backend,
    space: Backend::Space,
    error: InstallError<Backend::Error>,
) -> Result<T, InstallError<Backend::Error>> {
    match backend.abort(space) {
        Ok(()) => Err(error),
        Err(cleanup_error) => Err(InstallError::Cleanup(cleanup_error)),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallError<BackendError> {
    Backend(BackendError),
    Cleanup(BackendError),
    Loader(LoaderError),
    InvalidSegmentSize,
    VerificationFailed,
    WriteExecuteMapping,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DryRunSpace {
    generation: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DryRunMapping {
    slot: u8,
    generation: u32,
}

#[derive(Debug, Eq, PartialEq)]
pub struct ProcessImageHandle {
    slot: u16,
    generation: u32,
}

impl ProcessImageHandle {
    pub(crate) const fn new(slot: u16, generation: u32) -> Self {
        Self { slot, generation }
    }

    pub const fn slot(&self) -> u16 {
        self.slot
    }

    pub const fn generation(&self) -> u32 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessImageInfo {
    pub entry_point: u64,
    pub segment_count: usize,
    pub address_space_root: Option<u64>,
    pub owned_frames: usize,
}

#[derive(Clone, Copy)]
struct DryRunSlot<const BYTES: usize> {
    occupied: bool,
    sealed: bool,
    generation: u32,
    virtual_address: u64,
    memory_size: usize,
    permissions: MappingPermissions,
    bytes: [u8; BYTES],
}

impl<const BYTES: usize> DryRunSlot<BYTES> {
    const EMPTY: Self = Self {
        occupied: false,
        sealed: false,
        generation: 0,
        virtual_address: 0,
        memory_size: 0,
        permissions: MappingPermissions {
            readable: false,
            writable: false,
            executable: false,
        },
        bytes: [0; BYTES],
    };
}

/// Bounded software model for validating installer ordering during bootstrap.
///
/// It deliberately does not create hardware page tables or claim isolation.
pub struct DryRunAddressSpace<const BYTES_PER_SEGMENT: usize> {
    generation: u32,
    active: bool,
    image_start: u64,
    image_end: u64,
    slots: [DryRunSlot<BYTES_PER_SEGMENT>; MAXIMUM_PROCESS_SEGMENTS],
    slot_count: usize,
    process_live: bool,
    process_generation: u32,
    process_info: ProcessImageInfo,
}

impl<const BYTES_PER_SEGMENT: usize> DryRunAddressSpace<BYTES_PER_SEGMENT> {
    pub const fn new() -> Self {
        Self {
            generation: 0,
            active: false,
            image_start: 0,
            image_end: 0,
            slots: [const { DryRunSlot::EMPTY }; MAXIMUM_PROCESS_SEGMENTS],
            slot_count: 0,
            process_live: false,
            process_generation: 0,
            process_info: ProcessImageInfo {
                entry_point: 0,
                segment_count: 0,
                address_space_root: None,
                owned_frames: 0,
            },
        }
    }

    pub fn resolve_process(&self, handle: &ProcessImageHandle) -> Option<ProcessImageInfo> {
        (self.process_live && handle.slot == 0 && handle.generation == self.process_generation)
            .then_some(self.process_info)
    }

    pub fn release(&mut self, handle: &ProcessImageHandle) -> Result<(), DryRunError> {
        if self.resolve_process(handle).is_none() {
            return Err(DryRunError::InvalidHandle);
        }
        self.process_live = false;
        Ok(())
    }

    fn mapping_mut(
        &mut self,
        mapping: DryRunMapping,
    ) -> Result<&mut DryRunSlot<BYTES_PER_SEGMENT>, DryRunError> {
        self.slots
            .get_mut(usize::from(mapping.slot))
            .filter(|slot| slot.occupied && slot.generation == mapping.generation)
            .ok_or(DryRunError::InvalidHandle)
    }
}

impl<const BYTES_PER_SEGMENT: usize> Default for DryRunAddressSpace<BYTES_PER_SEGMENT> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const BYTES_PER_SEGMENT: usize> UserAddressSpaceBackend
    for DryRunAddressSpace<BYTES_PER_SEGMENT>
{
    type Error = DryRunError;
    type Space = DryRunSpace;
    type Mapping = DryRunMapping;
    type Process = ProcessImageHandle;

    fn begin(&mut self, image_start: u64, image_end: u64) -> Result<Self::Space, Self::Error> {
        if self.active || self.process_live || image_start >= image_end {
            return Err(DryRunError::BusyOrInvalid);
        }
        self.generation = next_generation(self.generation);
        self.active = true;
        self.image_start = image_start;
        self.image_end = image_end;
        self.slot_count = 0;
        self.slots.fill(DryRunSlot::EMPTY);
        Ok(DryRunSpace {
            generation: self.generation,
        })
    }

    fn map_zeroed(
        &mut self,
        space: Self::Space,
        virtual_address: u64,
        memory_size: usize,
    ) -> Result<Self::Mapping, Self::Error> {
        if !self.active
            || space.generation != self.generation
            || memory_size == 0
            || memory_size > BYTES_PER_SEGMENT
            || virtual_address < self.image_start
            || virtual_address
                .checked_add(memory_size as u64)
                .is_none_or(|end| end > self.image_end)
        {
            return Err(DryRunError::BusyOrInvalid);
        }
        let index = self.slot_count;
        let slot = self
            .slots
            .get_mut(index)
            .ok_or(DryRunError::CapacityExceeded)?;
        slot.occupied = true;
        slot.sealed = false;
        slot.generation = self.generation;
        slot.virtual_address = virtual_address;
        slot.memory_size = memory_size;
        slot.bytes.fill(0);
        self.slot_count += 1;
        Ok(DryRunMapping {
            slot: index as u8,
            generation: self.generation,
        })
    }

    fn copy_into(
        &mut self,
        mapping: Self::Mapping,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), Self::Error> {
        let slot = self.mapping_mut(mapping)?;
        if slot.sealed
            || offset
                .checked_add(bytes.len())
                .is_none_or(|end| end > slot.memory_size)
        {
            return Err(DryRunError::BusyOrInvalid);
        }
        slot.bytes[offset..offset + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    fn verify_contents(
        &mut self,
        mapping: Self::Mapping,
        initialized: &[u8],
        memory_size: usize,
    ) -> Result<bool, Self::Error> {
        let slot = self.mapping_mut(mapping)?;
        if memory_size != slot.memory_size || initialized.len() > memory_size {
            return Err(DryRunError::BusyOrInvalid);
        }
        Ok(slot.bytes[..initialized.len()] == *initialized
            && slot.bytes[initialized.len()..memory_size]
                .iter()
                .all(|byte| *byte == 0))
    }

    fn seal(
        &mut self,
        mapping: Self::Mapping,
        permissions: MappingPermissions,
    ) -> Result<(), Self::Error> {
        if permissions.writable && permissions.executable {
            return Err(DryRunError::WriteExecute);
        }
        let slot = self.mapping_mut(mapping)?;
        if slot.sealed {
            return Err(DryRunError::BusyOrInvalid);
        }
        slot.permissions = permissions;
        slot.sealed = true;
        Ok(())
    }

    fn commit(
        &mut self,
        space: Self::Space,
        entry_point: u64,
    ) -> Result<Self::Process, Self::Error> {
        if !self.active
            || space.generation != self.generation
            || self.slot_count == 0
            || self.slots[..self.slot_count]
                .iter()
                .any(|slot| !slot.sealed)
            || !self.slots[..self.slot_count].iter().any(|slot| {
                slot.permissions.executable
                    && entry_point >= slot.virtual_address
                    && entry_point < slot.virtual_address + slot.memory_size as u64
            })
        {
            return Err(DryRunError::BusyOrInvalid);
        }
        self.active = false;
        self.process_generation = next_generation(self.process_generation);
        self.process_live = true;
        self.process_info = ProcessImageInfo {
            entry_point,
            segment_count: self.slot_count,
            address_space_root: None,
            owned_frames: 0,
        };
        Ok(ProcessImageHandle::new(0, self.process_generation))
    }

    fn abort(&mut self, space: Self::Space) -> Result<(), Self::Error> {
        if self.active && space.generation == self.generation {
            self.active = false;
            self.slot_count = 0;
            self.slots.fill(DryRunSlot::EMPTY);
            Ok(())
        } else {
            Err(DryRunError::InvalidHandle)
        }
    }

    fn process_info(&self, process: &Self::Process) -> Option<ProcessImageInfo> {
        self.resolve_process(process)
    }

    fn process_generation(&self, process: &Self::Process) -> Option<u32> {
        self.resolve_process(process).map(|_| process.generation())
    }

    unsafe fn validate_activation(
        &mut self,
        process: &Self::Process,
        _authority: &Capability<'_, ProcessInstallControl>,
    ) -> Result<(), Self::Error> {
        self.resolve_process(process)
            .map(|_| ())
            .ok_or(DryRunError::InvalidHandle)
    }

    fn release_process(&mut self, process: &Self::Process) -> Result<(), Self::Error> {
        self.release(process)
    }
}

const fn next_generation(generation: u32) -> u32 {
    let next = generation.wrapping_add(1);
    if next == 0 { 1 } else { next }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DryRunError {
    BusyOrInvalid,
    CapacityExceeded,
    InvalidHandle,
    WriteExecute,
}

#[cfg(test)]
mod tests {
    use blacklab::oureboros::{
        FractalCatalog, FractalClass, FractalRecipe, FractalSeed, MINIMAL_X86_64_ELF_BYTES,
        TargetArchitecture, measure_recipe,
    };

    use crate::capability::{Authority, UserlandImageControl};
    use crate::process::image::prepare_user_image;

    use super::*;

    fn prepared<'bytes>(
        catalog: &FractalCatalog,
        bytes: &'bytes mut [u8; MINIMAL_X86_64_ELF_BYTES],
        image_control: &Capability<'_, UserlandImageControl>,
    ) -> PreparedUserImage<'bytes> {
        let artifact = catalog.materialize(1, bytes).unwrap();
        prepare_user_image(artifact, image_control).unwrap()
    }

    fn catalog() -> FractalCatalog {
        let recipe = FractalRecipe {
            algorithm_version: 2,
            base_entropy: 1,
            structural_mutator: 2,
        };
        let mut catalog = FractalCatalog::new();
        catalog
            .plant_seed(FractalSeed {
                inode_id: 1,
                class: FractalClass::Executable,
                architecture: TargetArchitecture::X86_64,
                recipe,
                unfolded_size_bytes: MINIMAL_X86_64_ELF_BYTES as u32,
                entry_offset: 128,
                expected_sha256: measure_recipe(recipe, MINIMAL_X86_64_ELF_BYTES).unwrap(),
            })
            .unwrap();
        catalog
    }

    #[test]
    fn installs_verifies_seals_and_releases_a_process_model() {
        let catalog = catalog();
        let mut bytes = [0_u8; MINIMAL_X86_64_ELF_BYTES];
        // SAFETY: Unit tests establish one isolated bootstrap authority.
        let authority = unsafe { Authority::assume_root() };
        let image_control = authority.grant::<UserlandImageControl>();
        let install_control = authority.grant::<ProcessInstallControl>();
        let image = prepared(&catalog, &mut bytes, &image_control);
        let mut backend = DryRunAddressSpace::<256>::new();
        let installed = install_user_image(image, &mut backend, &install_control).unwrap();
        assert_eq!(installed.entry_point, 0x1000);
        assert_eq!(installed.segment_count, 1);
        assert_eq!(
            backend.resolve_process(&installed.process),
            Some(ProcessImageInfo {
                entry_point: 0x1000,
                segment_count: 1,
                address_space_root: None,
                owned_frames: 0,
            })
        );
        // SAFETY: The dry-run backend has no privileged state and validates
        // only the committed handle lifecycle.
        unsafe {
            backend
                .validate_activation(&installed.process, &install_control)
                .unwrap();
        }
        backend.release(&installed.process).unwrap();
        assert_eq!(backend.resolve_process(&installed.process), None);
        // SAFETY: This is the same non-privileged dry-run lifecycle check.
        assert_eq!(
            unsafe { backend.validate_activation(&installed.process, &install_control) },
            Err(DryRunError::InvalidHandle)
        );
    }

    #[test]
    fn aborts_when_the_backend_cannot_hold_a_segment() {
        let catalog = catalog();
        let mut bytes = [0_u8; MINIMAL_X86_64_ELF_BYTES];
        // SAFETY: Unit tests establish one isolated bootstrap authority.
        let authority = unsafe { Authority::assume_root() };
        let image_control = authority.grant::<UserlandImageControl>();
        let install_control = authority.grant::<ProcessInstallControl>();
        let image = prepared(&catalog, &mut bytes, &image_control);
        let mut backend = DryRunAddressSpace::<2>::new();
        assert_eq!(
            install_user_image(image, &mut backend, &install_control),
            Err(InstallError::Backend(DryRunError::BusyOrInvalid))
        );
        assert!(!backend.active);
        assert_eq!(backend.slot_count, 0);
    }
}
