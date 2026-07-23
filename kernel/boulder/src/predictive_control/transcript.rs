//! Per-boot domain derivation from measured software and hardware transcripts.
//!
//! The output replaces repeated literal keys with distinct values bound to:
//!
//! - the measured Push image digest;
//! - an architecture counter sample;
//! - the enumerated PCI functions;
//! - Drivernet fingerprints, decisions, and committed resolutions.
//!
//! The transcript adds uniqueness and measurement binding. It is not a
//! substitute for platform entropy when cryptographic secrecy is required.

use super::hash::{HashError, Sha256, hkdf_expand, hmac_sha256};
use crate::drivers::drivernet::DriverNetSummary;
use crate::hw::pci::PciInventory;

const BOOT_DOMAIN_SALT: &[u8] = b"Sisyphus-OS boot domains v1";
const TRANSCRIPT_DOMAIN: &[u8] = b"Sisyphus-OS hardware transcript v1";
const MAXIMUM_DERIVED_VALUES: usize = 15;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DomainError {
    Hash(HashError),
    EmptyMeasurement,
    InvalidTranscript,
    ZeroDerivedValue,
    DomainCollision,
}

impl From<HashError> for DomainError {
    fn from(error: HashError) -> Self {
        Self::Hash(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CertifiedDomains {
    pub controller: u64,
    pub hodge: u64,
    pub optimization: u64,
    pub sheaf: u64,
    pub stabilizer: u64,
    pub persistence: u64,
    pub spectral: u64,
    pub tropical: u64,
    pub density: u64,
}

impl CertifiedDomains {
    pub const fn values(self) -> [u64; 9] {
        [
            self.controller,
            self.hodge,
            self.optimization,
            self.sheaf,
            self.stabilizer,
            self.persistence,
            self.spectral,
            self.tropical,
            self.density,
        ]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PredictiveSecrets {
    pub model: u64,
    pub conformal: u64,
    pub barrier: u64,
    pub planner: u64,
    pub certificate: u64,
}

impl PredictiveSecrets {
    pub const fn values(self) -> [u64; 5] {
        [
            self.model,
            self.conformal,
            self.barrier,
            self.planner,
            self.certificate,
        ]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootDomains {
    pub certified: CertifiedDomains,
    pub tensor: u64,
    pub predictive: PredictiveSecrets,
    pub transcript_root: [u8; 32],
    pub schedule_root: [u8; 32],
    pub boot_fingerprint: u64,
}

impl BootDomains {
    pub fn derive(
        measured_image_digest: [u8; 32],
        boot_counter: u64,
        inventory: &PciInventory,
        drivernet: &DriverNetSummary,
    ) -> Result<Self, DomainError> {
        if measured_image_digest.iter().all(|byte| *byte == 0) {
            return Err(DomainError::EmptyMeasurement);
        }
        if inventory.overflowed() || drivernet.length > drivernet.resolutions.len() {
            return Err(DomainError::InvalidTranscript);
        }

        let transcript_root = hardware_transcript(inventory, drivernet)?;
        let counter_bytes = boot_counter.to_le_bytes();
        let pseudo_random_key = hmac_sha256(
            BOOT_DOMAIN_SALT,
            &[&measured_image_digest, &counter_bytes, &transcript_root],
        )?;

        let controller = derive_u64(&pseudo_random_key, b"certified/controller")?;
        let hodge = derive_u64(&pseudo_random_key, b"certified/hodge")?;
        let optimization = derive_u64(&pseudo_random_key, b"certified/optimization")?;
        let sheaf = derive_u64(&pseudo_random_key, b"certified/sheaf")?;
        let stabilizer = derive_u64(&pseudo_random_key, b"certified/stabilizer")?;
        let persistence = derive_u64(&pseudo_random_key, b"certified/persistence")?;
        let spectral = derive_u64(&pseudo_random_key, b"certified/spectral")?;
        let tropical = derive_u64(&pseudo_random_key, b"certified/tropical")?;
        let density = derive_u64(&pseudo_random_key, b"certified/density")?;
        let tensor = derive_u64(&pseudo_random_key, b"tensor/runtime")?;
        let model = derive_u64(&pseudo_random_key, b"predictive/model")?;
        let conformal = derive_u64(&pseudo_random_key, b"predictive/conformal")?;
        let barrier = derive_u64(&pseudo_random_key, b"predictive/barrier")?;
        let planner = derive_u64(&pseudo_random_key, b"predictive/planner")?;
        let certificate = derive_u64(&pseudo_random_key, b"predictive/certificate")?;

        let certified = CertifiedDomains {
            controller,
            hodge,
            optimization,
            sheaf,
            stabilizer,
            persistence,
            spectral,
            tropical,
            density,
        };
        let predictive = PredictiveSecrets {
            model,
            conformal,
            barrier,
            planner,
            certificate,
        };

        let mut values = [0_u64; MAXIMUM_DERIVED_VALUES];
        values[..9].copy_from_slice(&certified.values());
        values[9] = tensor;
        values[10..].copy_from_slice(&predictive.values());
        validate_unique_nonzero(&values)?;

        let schedule_root = hmac_sha256(&pseudo_random_key, &[b"schedule/root", &transcript_root])?;
        let mut fingerprint_bytes = [0_u8; 8];
        fingerprint_bytes.copy_from_slice(&schedule_root[..8]);
        let boot_fingerprint = u64::from_le_bytes(fingerprint_bytes);

        Ok(Self {
            certified,
            tensor,
            predictive,
            transcript_root,
            schedule_root,
            boot_fingerprint,
        })
    }
}

pub fn hardware_transcript(
    inventory: &PciInventory,
    drivernet: &DriverNetSummary,
) -> Result<[u8; 32], DomainError> {
    let mut hash = Sha256::new();
    hash.update(TRANSCRIPT_DOMAIN)?;
    hash.update_u64(inventory.devices().len() as u64)?;
    hash.update_u8(u8::from(inventory.overflowed()))?;

    for device in inventory.devices() {
        hash.update_u8(device.address.bus)?;
        hash.update_u8(device.address.slot)?;
        hash.update_u8(device.address.function)?;
        hash.update_u16(device.vendor_id)?;
        hash.update_u16(device.device_id)?;
        hash.update_u8(device.class_code)?;
        hash.update_u8(device.subclass)?;
        hash.update_u8(device.programming_interface)?;
        hash.update_u8(device.revision)?;
        hash.update_u8(device.header_type)?;
        hash.update_u8(device.interrupt_line)?;
        hash.update_u8(device.interrupt_pin)?;
    }

    hash.update_u64(drivernet.length as u64)?;
    hash.update_u64(
        drivernet
            .primary_index
            .map(|index| index as u64)
            .unwrap_or(u64::MAX),
    )?;
    hash.update_u64(drivernet.native_count as u64)?;
    hash.update_u64(drivernet.firmware_count as u64)?;
    hash.update_u64(drivernet.quarantined_count as u64)?;
    hash.update_u64(drivernet.failed_count as u64)?;
    hash.update_u8(u8::from(drivernet.display_available))?;
    hash.update_u64(drivernet.summary_root)?;
    hash.update_u64(drivernet.fingerprint_summary.length as u64)?;
    hash.update_u64(drivernet.fingerprint_summary.display_functions as u64)?;
    hash.update_u8(u8::from(drivernet.fingerprint_summary.inventory_overflowed))?;
    hash.update_u64(drivernet.fingerprint_summary.configuration_faults as u64)?;
    hash.update_u8(u8::from(
        drivernet.fingerprint_summary.synthetic_firmware_entry,
    ))?;

    for resolution in drivernet.resolutions() {
        hash.update_u8(resolution.fingerprint.bus)?;
        hash.update_u8(resolution.fingerprint.slot)?;
        hash.update_u8(resolution.fingerprint.function)?;
        hash.update_u16(resolution.fingerprint.vendor_id)?;
        hash.update_u16(resolution.fingerprint.device_id)?;
        hash.update_u16(resolution.confidence_q16)?;
        hash.update_u64(resolution.framebuffer_object)?;
        hash.update_u64(resolution.driver_handle)?;
        hash.update_u32(resolution.driver_generation)?;
        hash.update_u64(resolution.decision_root)?;
        hash.update_u64(resolution.resolution_root)?;
        hash.update_u8(resolution.strategy as u8)?;
        hash.update_u8(resolution.status as u8)?;
        hash.update_u16(resolution.fault as u16)?;
    }

    Ok(hash.finalize())
}

fn derive_u64(pseudo_random_key: &[u8; 32], label: &[u8]) -> Result<u64, DomainError> {
    let mut output = [0_u8; 8];
    hkdf_expand(pseudo_random_key, label, &mut output)?;
    let value = u64::from_le_bytes(output);
    if value == 0 {
        Err(DomainError::ZeroDerivedValue)
    } else {
        Ok(value)
    }
}

fn validate_unique_nonzero(values: &[u64; MAXIMUM_DERIVED_VALUES]) -> Result<(), DomainError> {
    for left in 0..values.len() {
        if values[left] == 0 {
            return Err(DomainError::ZeroDerivedValue);
        }
        for right in left + 1..values.len() {
            if values[left] == values[right] {
                return Err(DomainError::DomainCollision);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_labels_are_distinct() {
        let key = [7_u8; 32];
        let left = derive_u64(&key, b"left").unwrap();
        let right = derive_u64(&key, b"right").unwrap();
        assert_ne!(left, right);
        assert_ne!(left, 0);
        assert_ne!(right, 0);
    }

    #[test]
    fn schedule_changes_with_counter() {
        let measured = [1_u8; 32];
        let transcript = [2_u8; 32];

        let a = hmac_sha256(
            BOOT_DOMAIN_SALT,
            &[&measured, &1_u64.to_le_bytes(), &transcript],
        )
        .unwrap();
        let b = hmac_sha256(
            BOOT_DOMAIN_SALT,
            &[&measured, &2_u64.to_le_bytes(), &transcript],
        )
        .unwrap();

        assert_ne!(a, b);
    }
}
