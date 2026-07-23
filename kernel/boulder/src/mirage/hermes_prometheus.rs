use sisyphus_driver_abi::prometheus::{CallingConv, PrologueDecoder};

use crate::mirage::morphic_x86_64::{MorphicError, ScalarCallContract, X64Abi, emit_scalar_bridge};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForeignDispatchManifest {
    pub source_abi: X64Abi,
    pub loaded_address: u64,
    pub file_offset: u64,
    pub function_size: u32,
    pub contract_hash: [u8; 32],
    pub authority_epoch: u64,
    pub target_has_endbr64: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbiEvidence {
    ManifestAndPrologue,
    ManifestOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MorphicObservation {
    pub detected: CallingConv,
    pub stack_frame_size: u32,
    pub saves_shadow: bool,
    pub uses_red_zone: bool,
    pub evidence: AbiEvidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HermesDispatchBridgePlan {
    pub source_abi: X64Abi,
    pub target_abi: X64Abi,
    pub target_address: u64,
    pub contract: ScalarCallContract,
    pub observation: MorphicObservation,
}

impl HermesDispatchBridgePlan {
    pub fn emit(&self, output: &mut [u8]) -> Result<usize, BridgeError> {
        emit_scalar_bridge(
            output,
            self.source_abi,
            self.target_abi,
            self.target_address,
            self.contract,
        )
        .map_err(BridgeError::Morphic)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BridgeError {
    InvalidManifest,
    InvalidEntrypoint,
    UnsupportedArchitecture,
    AbiContradiction,
    Morphic(MorphicError),
}

/// Plans the single three-argument dispatch thunk used by a foreign Hermes
/// personality. The kernel calls the thunk using System V. The thunk enters the
/// personality using its declared ABI.
///
/// Prologue analysis is corroborating evidence only. The authority-signed
/// manifest is the contract; contradictory machine-code evidence fails closed.
pub fn plan_hermes_dispatch(
    object_bytes: &[u8],
    manifest: ForeignDispatchManifest,
    emit_endbr64: bool,
) -> Result<HermesDispatchBridgePlan, BridgeError> {
    validate_manifest(&manifest)?;

    let file_offset =
        usize::try_from(manifest.file_offset).map_err(|_| BridgeError::InvalidEntrypoint)?;
    let declared_size = manifest.function_size as usize;
    let available = object_bytes
        .get(file_offset..)
        .ok_or(BridgeError::InvalidEntrypoint)?;
    let prologue_length = available.len().min(declared_size).min(64);
    if prologue_length == 0 {
        return Err(BridgeError::InvalidEntrypoint);
    }

    let decoded = PrologueDecoder::decode(&available[..prologue_length]);
    let evidence = corroborate(manifest.source_abi, decoded.detected)?;

    let contract = ScalarCallContract {
        integer_arguments: 3,
        has_vector_arguments: false,
        variadic: false,
        may_unwind: false,
        target_has_endbr64: manifest.target_has_endbr64,
        emit_endbr64,
    };

    Ok(HermesDispatchBridgePlan {
        source_abi: X64Abi::SystemV,
        target_abi: manifest.source_abi,
        target_address: manifest.loaded_address,
        contract,
        observation: MorphicObservation {
            detected: decoded.detected,
            stack_frame_size: decoded.stack_frame_size,
            saves_shadow: decoded.saves_shadow,
            uses_red_zone: decoded.uses_red_zone,
            evidence,
        },
    })
}

fn validate_manifest(manifest: &ForeignDispatchManifest) -> Result<(), BridgeError> {
    if manifest.loaded_address == 0
        || manifest.function_size == 0
        || manifest.authority_epoch == 0
        || manifest.contract_hash == [0; 32]
    {
        return Err(BridgeError::InvalidManifest);
    }
    Ok(())
}

fn corroborate(declared: X64Abi, detected: CallingConv) -> Result<AbiEvidence, BridgeError> {
    match (declared, detected) {
        (X64Abi::SystemV, CallingConv::SysVAmd64) | (X64Abi::Microsoft, CallingConv::MsX64) => {
            Ok(AbiEvidence::ManifestAndPrologue)
        }

        (_, CallingConv::Unknown) => Ok(AbiEvidence::ManifestOnly),

        (X64Abi::SystemV, CallingConv::MsX64) | (X64Abi::Microsoft, CallingConv::SysVAmd64) => {
            Err(BridgeError::AbiContradiction)
        }

        (_, CallingConv::Cdecl32 | CallingConv::Stdcall32) => {
            Err(BridgeError::UnsupportedArchitecture)
        }

        (_, CallingConv::Aapcs64 | CallingConv::RiscVLp64) => {
            Err(BridgeError::UnsupportedArchitecture)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_manifest_without_authority() {
        let manifest = ForeignDispatchManifest {
            source_abi: X64Abi::Microsoft,
            loaded_address: 0x1000,
            file_offset: 0,
            function_size: 8,
            contract_hash: [1; 32],
            authority_epoch: 0,
            target_has_endbr64: false,
        };

        assert_eq!(
            plan_hermes_dispatch(&[0x48, 0x83, 0xec, 0x28, 0xc3], manifest, false),
            Err(BridgeError::InvalidManifest)
        );
    }

    #[test]
    fn detects_a_declared_abi_contradiction() {
        let manifest = ForeignDispatchManifest {
            source_abi: X64Abi::SystemV,
            loaded_address: 0x1000,
            file_offset: 0,
            function_size: 5,
            contract_hash: [1; 32],
            authority_epoch: 1,
            target_has_endbr64: false,
        };

        assert_eq!(
            plan_hermes_dispatch(&[0x48, 0x83, 0xec, 0x28, 0xc3], manifest, false),
            Err(BridgeError::AbiContradiction)
        );
    }

    #[test]
    fn plans_a_three_argument_windows_dispatch_thunk() {
        let manifest = ForeignDispatchManifest {
            source_abi: X64Abi::Microsoft,
            loaded_address: 0x1000,
            file_offset: 0,
            function_size: 5,
            contract_hash: [1; 32],
            authority_epoch: 1,
            target_has_endbr64: false,
        };

        let plan = plan_hermes_dispatch(&[0x48, 0x83, 0xec, 0x28, 0xc3], manifest, false).unwrap();

        assert_eq!(plan.source_abi, X64Abi::SystemV);
        assert_eq!(plan.target_abi, X64Abi::Microsoft);
        assert_eq!(plan.contract.integer_arguments, 3);
        assert_eq!(plan.observation.evidence, AbiEvidence::ManifestAndPrologue);
    }
}
