//! Build-bound measurements of the checked dependent-type models.
//!
//! The bare-metal build accepts these roots only after the formal gate has
//! emitted a matching attestation. They are then folded into PID1's capability
//! root, binding user authority to the exact driver, package, and privilege
//! models checked for this kernel image.

pub const FORMAL_SCHEMA_VERSION: u16 = 1;

const DRIVER_LIFECYCLE_SHA256: [u8; 32] = parse_sha256(env!("SISYPHUS_DRIVER_PROOF_SHA256"));
const PACKAGE_TRANSACTION_SHA256: [u8; 32] = parse_sha256(env!("SISYPHUS_PACKAGE_PROOF_SHA256"));
const PRIVILEGE_RINGS_SHA256: [u8; 32] = parse_sha256(env!("SISYPHUS_PRIVILEGE_PROOF_SHA256"));

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FormalAttestation {
    pub schema_version: u16,
    pub driver_lifecycle_sha256: [u8; 32],
    pub package_transaction_sha256: [u8; 32],
    pub privilege_rings_sha256: [u8; 32],
    pub authority_root: u64,
}

impl FormalAttestation {
    pub const fn current() -> Self {
        let authority_root = fold_roots(
            DRIVER_LIFECYCLE_SHA256,
            PACKAGE_TRANSACTION_SHA256,
            PRIVILEGE_RINGS_SHA256,
        );
        Self {
            schema_version: FORMAL_SCHEMA_VERSION,
            driver_lifecycle_sha256: DRIVER_LIFECYCLE_SHA256,
            package_transaction_sha256: PACKAGE_TRANSACTION_SHA256,
            privilege_rings_sha256: PRIVILEGE_RINGS_SHA256,
            authority_root,
        }
    }

    pub const fn validate(self) -> bool {
        self.schema_version == FORMAL_SCHEMA_VERSION
            && !all_zero(self.driver_lifecycle_sha256)
            && !all_zero(self.package_transaction_sha256)
            && !all_zero(self.privilege_rings_sha256)
            && !equal_digest(
                self.driver_lifecycle_sha256,
                self.package_transaction_sha256,
            )
            && !equal_digest(self.driver_lifecycle_sha256, self.privilege_rings_sha256)
            && !equal_digest(self.package_transaction_sha256, self.privilege_rings_sha256)
            && self.authority_root
                == fold_roots(
                    self.driver_lifecycle_sha256,
                    self.package_transaction_sha256,
                    self.privilege_rings_sha256,
                )
            && self.authority_root != 0
    }
}

const fn fold_roots(driver: [u8; 32], package: [u8; 32], privilege: [u8; 32]) -> u64 {
    let mut state = 0x5349_5359_5048_5553_u64;
    let roots = [driver, package, privilege];
    let mut root_index = 0;
    while root_index < roots.len() {
        let mut word_index = 0;
        while word_index < 4 {
            let offset = word_index * 8;
            let word = u64::from_le_bytes([
                roots[root_index][offset],
                roots[root_index][offset + 1],
                roots[root_index][offset + 2],
                roots[root_index][offset + 3],
                roots[root_index][offset + 4],
                roots[root_index][offset + 5],
                roots[root_index][offset + 6],
                roots[root_index][offset + 7],
            ]);
            state ^= word.rotate_left(((root_index * 17 + word_index * 11) % 64) as u32);
            state = state.wrapping_mul(0x9e37_79b1_85eb_ca87).rotate_left(23);
            word_index += 1;
        }
        root_index += 1;
    }
    state
}

const fn all_zero(bytes: [u8; 32]) -> bool {
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != 0 {
            return false;
        }
        index += 1;
    }
    true
}

const fn equal_digest(left: [u8; 32], right: [u8; 32]) -> bool {
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

const fn parse_sha256(encoded: &str) -> [u8; 32] {
    let bytes = encoded.as_bytes();
    assert!(
        bytes.len() == 64,
        "formal digest must contain 64 hexadecimal digits"
    );
    let mut digest = [0_u8; 32];
    let mut index = 0;
    while index < digest.len() {
        digest[index] = (hex_nibble(bytes[index * 2]) << 4) | hex_nibble(bytes[index * 2 + 1]);
        index += 1;
    }
    digest
}

const fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => panic!("formal digest contains a non-hexadecimal byte"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_models_form_one_non_aliasing_authority_root() {
        let attestation = FormalAttestation::current();
        assert!(attestation.validate());
        assert_ne!(attestation.authority_root, 0);
    }

    #[test]
    fn any_root_mutation_invalidates_the_attestation() {
        let mut attestation = FormalAttestation::current();
        attestation.privilege_rings_sha256[0] ^= 1;
        assert!(!attestation.validate());
    }
}
