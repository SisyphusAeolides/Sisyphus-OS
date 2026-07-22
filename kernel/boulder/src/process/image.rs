use blacklab::oureboros::{
    ArtifactMeasurement, FractalClass, TargetArchitecture, VerifiedArtifact,
};

use crate::capability::{Capability, UserlandImageControl};
use crate::module::loader::{LoadPlan, LoaderError};

pub const MINIMUM_USER_ADDRESS: u64 = 0x1000;
pub const USER_ADDRESS_LIMIT: u64 = 0x0000_8000_0000_0000;
pub const MAXIMUM_USER_IMAGE_SPAN: u64 = 64 * 1024 * 1024;

/// A measured image and its validated static load plan.
///
/// The artifact remains immutably borrowed for this object's lifetime. A
/// The address-space installer consumes this object, copies each segment into
/// inaccessible zeroed staging memory, verifies initialized data and BSS, and
/// only then seals final permissions. Static relocations remain unsupported.
pub struct PreparedUserImage<'bytes> {
    artifact: VerifiedArtifact<'bytes>,
    plan: LoadPlan,
}

impl PreparedUserImage<'_> {
    pub const fn measurement(&self) -> ArtifactMeasurement {
        self.artifact.measurement()
    }

    pub const fn plan(&self) -> &LoadPlan {
        &self.plan
    }

    pub const fn bytes(&self) -> &[u8] {
        self.artifact.bytes()
    }
}

pub fn prepare_user_image<'bytes>(
    artifact: VerifiedArtifact<'bytes>,
    _authority: &Capability<'_, UserlandImageControl>,
) -> Result<PreparedUserImage<'bytes>, UserImageError> {
    let measurement = artifact.measurement();
    if measurement.class != FractalClass::Executable {
        return Err(UserImageError::WrongClass);
    }
    if measurement.architecture != TargetArchitecture::X86_64 {
        return Err(UserImageError::WrongArchitecture);
    }
    if measurement.bytes_written != artifact.bytes().len() {
        return Err(UserImageError::MeasurementMismatch);
    }

    let plan = LoadPlan::parse(artifact.bytes()).map_err(UserImageError::Loader)?;
    if plan.requires_runtime_linker {
        return Err(UserImageError::RuntimeLinkerUnavailable);
    }
    let image_span = plan
        .image_end
        .checked_sub(plan.image_start)
        .ok_or(UserImageError::InvalidUserRange)?;
    if plan.image_start < MINIMUM_USER_ADDRESS
        || plan.image_end > USER_ADDRESS_LIMIT
        || image_span == 0
        || image_span > MAXIMUM_USER_IMAGE_SPAN
    {
        return Err(UserImageError::InvalidUserRange);
    }
    if plan
        .segments()
        .iter()
        .any(|segment| segment.executable && !segment.readable)
    {
        return Err(UserImageError::UnreadableCode);
    }
    let entry_file_offset = plan.entry_file_offset().map_err(UserImageError::Loader)?;
    if entry_file_offset != measurement.entry_offset {
        return Err(UserImageError::EntryMetadataMismatch);
    }

    Ok(PreparedUserImage { artifact, plan })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserImageError {
    WrongClass,
    WrongArchitecture,
    MeasurementMismatch,
    RuntimeLinkerUnavailable,
    InvalidUserRange,
    UnreadableCode,
    EntryMetadataMismatch,
    Loader(LoaderError),
}

#[cfg(test)]
mod tests {
    use blacklab::oureboros::{
        FractalCatalog, FractalRecipe, FractalSeed, MINIMAL_X86_64_ELF_BYTES, measure_recipe,
    };

    use crate::capability::Authority;

    use super::*;

    fn recipe() -> FractalRecipe {
        FractalRecipe {
            algorithm_version: 2,
            base_entropy: 0x9999_8888_7777_6666,
            structural_mutator: 0xaaaa_bbbb_cccc_dddd,
        }
    }

    #[test]
    fn binds_a_measured_artifact_to_a_static_user_load_plan() {
        let recipe = recipe();
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
        let mut bytes = [0_u8; MINIMAL_X86_64_ELF_BYTES];
        let artifact = catalog.materialize(1, &mut bytes).unwrap();
        // SAFETY: Unit tests establish one isolated bootstrap authority.
        let authority = unsafe { Authority::assume_root() };
        let image_control = authority.grant::<UserlandImageControl>();
        let prepared = prepare_user_image(artifact, &image_control).unwrap();
        assert_eq!(prepared.plan().entry_point, 0x1000);
        assert_eq!(prepared.plan().entry_file_offset(), Ok(128));
        assert_eq!(&prepared.bytes()[162..], b"PID1 syscall write\n");
        assert_eq!(&prepared.bytes()[128..133], &[0xb8, 1, 0, 0, 0]);
        assert_eq!(&prepared.bytes()[152..157], &[0xb8, 17, 0, 0, 0]);
    }

    #[test]
    fn refuses_non_executable_artifacts() {
        let recipe = FractalRecipe {
            algorithm_version: 1,
            base_entropy: 1,
            structural_mutator: 2,
        };
        let mut catalog = FractalCatalog::new();
        catalog
            .plant_seed(FractalSeed {
                inode_id: 2,
                class: FractalClass::Configuration,
                architecture: TargetArchitecture::Independent,
                recipe,
                unfolded_size_bytes: 8,
                entry_offset: 0,
                expected_sha256: measure_recipe(recipe, 8).unwrap(),
            })
            .unwrap();
        let mut bytes = [0_u8; 8];
        let artifact = catalog.materialize(2, &mut bytes).unwrap();
        // SAFETY: Unit tests establish one isolated bootstrap authority.
        let authority = unsafe { Authority::assume_root() };
        let image_control = authority.grant::<UserlandImageControl>();
        assert!(matches!(
            prepare_user_image(artifact, &image_control),
            Err(UserImageError::WrongClass)
        ));
    }
}
