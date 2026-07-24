//! Halted-controller xHCI ring programming and polled command preparation.
//!
//! This is the narrow bridge between retained reset-ready evidence and a
//! future one-shot runtime. It programs only while HCHalted is proven and
//! never treats missing DMAR data as an identity-DMA witness. Bus mastering,
//! Run/Stop, and teardown remain separate transactions with their own debt.

use super::xhci::XhciRuntimeSeed;
use super::xhci_dma::{XhciDmaPurpose, XhciDmaRegionPhase, XhciDmaRegionRecord};
use super::xhci_iommu::{XhciIommuBindFailure, XhciIommuBinding, bind_core_regions};
use super::xhci_ring::{
    XhciCommandCompletionEvidence, XhciNoOpCommandReceipt, XhciRingError, XhciRingMachine,
    XhciRingStorage,
};

const USBCMD_RUN_STOP: u32 = 1 << 0;
const USBSTS_HCHALTED: u32 = 1 << 0;
const USBSTS_HOST_CONTROLLER_ERROR: u32 = 1 << 2;
const EVENT_HANDLER_BUSY: u64 = 1 << 3;
const OP_CRCR: u32 = 0x18;
const OP_DCBAAP: u32 = 0x30;
const OP_CONFIG: u32 = 0x38;
const RT_ERSTSZ: u32 = 0x28;
const RT_ERSTBA: u32 = 0x30;
const RT_ERDP: u32 = 0x38;
const RUNTIME_INTERRUPTER: u32 = 0x20;
const RUNTIME_REGISTER_BYTES: u32 = 0x40;
const DOORBELL_BYTES: u32 = 4;
const RUNTIME_ROOT_DOMAIN: u64 = 0x5848_4349_5254_4d45;

/// Narrow register contract for the halted programming epoch.
pub trait XhciRuntimeRegisters {
    type Error;

    fn read32(&mut self, offset: u32) -> Result<u32, Self::Error>;
    fn write32(&mut self, offset: u32, value: u32) -> Result<(), Self::Error>;
    fn write64(&mut self, offset: u32, value: u64) -> Result<(), Self::Error>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XhciRuntimeInvariant {
    InvalidSecret,
    ControllerNotHalted,
    ControllerError,
    MissingDmaRegion(XhciDmaPurpose),
    DmaRegionNotReady(XhciDmaPurpose),
    DmaGenerationMismatch(XhciDmaPurpose),
    DmaRegionGeometry(XhciDmaPurpose),
    AddressExceeds32Bit(u64),
    RegisterOffsetOverflow,
}

#[derive(Debug, Eq, PartialEq)]
pub enum XhciRuntimeError<StorageError, RegisterError> {
    Invariant(XhciRuntimeInvariant),
    Ring(XhciRingError<StorageError>),
    Register(RegisterError),
}

/// The result of programming the controller's address-bearing ring registers
/// while it is still halted. No command has been submitted and no bus-master
/// bit has been enabled by this object.
pub struct XhciPreparedRuntime {
    rings: XhciRingMachine,
    generation: u32,
    runtime_offset: u32,
    doorbell_offset: u32,
    event_dequeue_pointer: u64,
    runtime_root: u64,
}

impl XhciPreparedRuntime {
    pub const fn generation(&self) -> u32 {
        self.generation
    }

    pub const fn runtime_root(&self) -> u64 {
        self.runtime_root
    }

    pub const fn ring_root(&self) -> u64 {
        self.rings.ring_root()
    }

    pub const fn event_dequeue_pointer(&self) -> u64 {
        self.event_dequeue_pointer
    }

    /// Installs exact requester mappings for the already-prepared arena.
    /// This does not enable bus mastering; the caller must retain the binding
    /// and complete the controller's separate BM/Run-Stop transaction.
    pub fn bind_translated_dma<S>(
        &self,
        domain: &mut crate::hw::iommu::IommuDomain<'_>,
        storage: &S,
        secret: u64,
    ) -> Result<XhciIommuBinding, XhciIommuBindFailure>
    where
        S: XhciRingStorage,
    {
        bind_core_regions(domain, storage, self.generation, secret)
    }

    pub fn submit_no_op<S, R>(
        &mut self,
        storage: &S,
        registers: &mut R,
    ) -> Result<XhciNoOpCommandReceipt, XhciRuntimeError<S::Error, R::Error>>
    where
        S: XhciRingStorage,
        R: XhciRuntimeRegisters,
    {
        let receipt = self
            .rings
            .submit_no_op(storage)
            .map_err(XhciRuntimeError::Ring)?;
        registers
            .write32(self.doorbell_offset, 0)
            .map_err(XhciRuntimeError::Register)?;
        Ok(receipt)
    }

    pub fn poll_no_op_completion<S, R>(
        &mut self,
        storage: &S,
        registers: &mut R,
        receipt: &XhciNoOpCommandReceipt,
    ) -> Result<Option<XhciCommandCompletionEvidence>, XhciRuntimeError<S::Error, R::Error>>
    where
        S: XhciRingStorage,
        R: XhciRuntimeRegisters,
    {
        let completion = self
            .rings
            .poll_no_op_completion(storage, receipt)
            .map_err(XhciRuntimeError::Ring)?;
        if let Some(evidence) = completion {
            let dequeue = evidence.next_event_dequeue_pointer | EVENT_HANDLER_BUSY;
            registers
                .write64(self.event_dequeue_pointer_offset(), dequeue)
                .map_err(XhciRuntimeError::Register)?;
            self.event_dequeue_pointer = evidence.next_event_dequeue_pointer;
        }
        Ok(completion)
    }

    fn event_dequeue_pointer_offset(&self) -> u32 {
        self.runtime_offset + RUNTIME_INTERRUPTER + RT_ERDP
    }
}

pub fn prepare_halted<S, R>(
    seed: &XhciRuntimeSeed,
    storage: &S,
    registers: &mut R,
    secret: u64,
) -> Result<XhciPreparedRuntime, XhciRuntimeError<S::Error, R::Error>>
where
    S: XhciRingStorage,
    R: XhciRuntimeRegisters,
{
    if secret == 0 {
        return Err(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::InvalidSecret,
        ));
    }
    let evidence = seed.evidence();
    let operational_offset = u32::from(evidence.snapshot.capability_length);
    let status_offset = checked_offset(operational_offset, 4)?;
    let command = registers
        .read32(operational_offset)
        .map_err(XhciRuntimeError::Register)?;
    let status = registers
        .read32(status_offset)
        .map_err(XhciRuntimeError::Register)?;
    if command & USBCMD_RUN_STOP != 0 || status & USBSTS_HCHALTED == 0 {
        return Err(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::ControllerNotHalted,
        ));
    }
    if status & USBSTS_HOST_CONTROLLER_ERROR != 0 {
        return Err(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::ControllerError,
        ));
    }

    let dcbaa = ready_region(storage, XhciDmaPurpose::Dcbaa, evidence.generation)?;
    let erst = ready_region(
        storage,
        XhciDmaPurpose::EventRingSegmentTable,
        evidence.generation,
    )?;
    let crcr = checked_offset(operational_offset, OP_CRCR)?;
    let dcbaap = checked_offset(operational_offset, OP_DCBAAP)?;
    let config = checked_offset(operational_offset, OP_CONFIG)?;
    let runtime_offset = evidence.snapshot.runtime_offset;
    let runtime_base = runtime_offset;
    let runtime_end = runtime_base
        .checked_add(RUNTIME_INTERRUPTER)
        .and_then(|offset| offset.checked_add(RUNTIME_REGISTER_BYTES))
        .ok_or(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::RegisterOffsetOverflow,
        ))?;
    if u64::from(runtime_end) > seed.aperture().length() {
        return Err(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::RegisterOffsetOverflow,
        ));
    }
    let erstsz = runtime_base
        .checked_add(RUNTIME_INTERRUPTER)
        .and_then(|offset| offset.checked_add(RT_ERSTSZ))
        .ok_or(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::RegisterOffsetOverflow,
        ))?;
    let erstba = erstsz
        .checked_add(RT_ERSTBA - RT_ERSTSZ)
        .ok_or(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::RegisterOffsetOverflow,
        ))?;
    let erdp = erstsz
        .checked_add(RT_ERDP - RT_ERSTSZ)
        .ok_or(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::RegisterOffsetOverflow,
        ))?;
    let doorbell_end = evidence
        .snapshot
        .doorbell_offset
        .checked_add(DOORBELL_BYTES)
        .ok_or(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::RegisterOffsetOverflow,
        ))?;
    if u64::from(doorbell_end) > seed.aperture().length() {
        return Err(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::RegisterOffsetOverflow,
        ));
    }
    let doorbell = evidence.snapshot.doorbell_offset;

    // Finish every aperture/register validation before touching DMA storage.
    // If the transaction is rejected, no ring metadata has been published.
    let rings = XhciRingMachine::initialize_for_generation(storage, evidence.generation, secret)
        .map_err(XhciRuntimeError::Ring)?;
    let program = rings.register_program();

    write_address(
        registers,
        dcbaap,
        dcbaa.device_address_start,
        evidence.snapshot.supports_64_bit_addresses,
    )?;
    registers
        .write32(config, u32::from(evidence.snapshot.maximum_device_slots))
        .map_err(XhciRuntimeError::Register)?;
    write_address(
        registers,
        crcr,
        program.command_ring_control,
        evidence.snapshot.supports_64_bit_addresses,
    )?;
    registers
        .write32(erstsz, 1)
        .map_err(XhciRuntimeError::Register)?;
    write_address(
        registers,
        erstba,
        erst.device_address_start,
        evidence.snapshot.supports_64_bit_addresses,
    )?;
    write_address(
        registers,
        erdp,
        program.event_ring_dequeue_pointer,
        evidence.snapshot.supports_64_bit_addresses,
    )?;

    let mut root = mix(secret ^ RUNTIME_ROOT_DOMAIN, evidence.reset_ready_root);
    root = mix(root, evidence.generation.into());
    root = mix(root, rings.ring_root());
    root = mix(root, dcbaa.region_root);
    root = mix(root, erst.region_root);
    root = mix(root, u64::from(operational_offset));
    root = mix(root, u64::from(evidence.snapshot.runtime_offset));
    root = mix(root, u64::from(doorbell));
    Ok(XhciPreparedRuntime {
        rings,
        generation: evidence.generation,
        runtime_offset,
        doorbell_offset: doorbell,
        event_dequeue_pointer: program.event_ring_dequeue_pointer,
        runtime_root: canonical_root(root),
    })
}

fn ready_region<S: XhciRingStorage, R>(
    storage: &S,
    purpose: XhciDmaPurpose,
    generation: u32,
) -> Result<XhciDmaRegionRecord, XhciRuntimeError<S::Error, R>> {
    let Some(record) = storage.region(purpose) else {
        return Err(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::MissingDmaRegion(purpose),
        ));
    };
    if record.phase != XhciDmaRegionPhase::Ready {
        return Err(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::DmaRegionNotReady(purpose),
        ));
    }
    if record.generation != generation {
        return Err(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::DmaGenerationMismatch(purpose),
        ));
    }
    let Some(expected_end) = record.device_address_start.checked_add(4096) else {
        return Err(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::DmaRegionGeometry(purpose),
        ));
    };
    if record.page_count != 1
        || record.device_address_start % 4096 != 0
        || record.device_address_end != expected_end
    {
        return Err(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::DmaRegionGeometry(purpose),
        ));
    }
    Ok(record)
}

fn write_address<S, R: XhciRuntimeRegisters>(
    registers: &mut R,
    offset: u32,
    address: u64,
    supports_64_bit_addresses: bool,
) -> Result<(), XhciRuntimeError<S, R::Error>> {
    if supports_64_bit_addresses {
        registers
            .write64(offset, address)
            .map_err(XhciRuntimeError::Register)
    } else {
        let low = u32::try_from(address).map_err(|_| {
            XhciRuntimeError::Invariant(XhciRuntimeInvariant::AddressExceeds32Bit(address))
        })?;
        registers
            .write32(offset, low)
            .map_err(XhciRuntimeError::Register)
    }
}

fn checked_offset<S, R>(base: u32, displacement: u32) -> Result<u32, XhciRuntimeError<S, R>> {
    base.checked_add(displacement)
        .ok_or(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::RegisterOffsetOverflow,
        ))
}

const fn canonical_root(root: u64) -> u64 {
    if root == 0 { RUNTIME_ROOT_DOMAIN } else { root }
}

fn mix(mut state: u64, word: u64) -> u64 {
    state ^= word.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RegisterProbe {
        read_value: u32,
        last_offset: u32,
        last_value: u64,
        writes: u8,
    }

    impl XhciRuntimeRegisters for RegisterProbe {
        type Error = ();

        fn read32(&mut self, _offset: u32) -> Result<u32, Self::Error> {
            Ok(self.read_value)
        }

        fn write32(&mut self, offset: u32, value: u32) -> Result<(), Self::Error> {
            self.last_offset = offset;
            self.last_value = u64::from(value);
            self.writes = self.writes.saturating_add(1);
            Ok(())
        }

        fn write64(&mut self, offset: u32, value: u64) -> Result<(), Self::Error> {
            self.last_offset = offset;
            self.last_value = value;
            self.writes = self.writes.saturating_add(1);
            Ok(())
        }
    }

    #[test]
    fn address_programming_obeys_ac64_without_touching_the_high_dword() {
        let mut probe = RegisterProbe::default();
        write_address::<(), _>(&mut probe, 0x30, 0x0123_4567_89ab_cdef, true).unwrap();
        assert_eq!(probe.last_offset, 0x30);
        assert_eq!(probe.last_value, 0x0123_4567_89ab_cdef);
        assert_eq!(probe.writes, 1);

        write_address::<(), _>(&mut probe, 0x30, 0x0000_0000_89ab_cdef, false).unwrap();
        assert_eq!(probe.last_value, 0x89ab_cdef);
        assert_eq!(probe.writes, 2);
        assert_eq!(
            write_address::<(), _>(&mut probe, 0x30, 0x1_0000_0000, false),
            Err(XhciRuntimeError::Invariant(
                XhciRuntimeInvariant::AddressExceeds32Bit(0x1_0000_0000)
            ))
        );
        assert_eq!(probe.writes, 2);
    }

    #[test]
    fn register_offsets_fail_closed_on_wrap() {
        assert_eq!(
            checked_offset::<(), ()>(u32::MAX, 1),
            Err(XhciRuntimeError::Invariant(
                XhciRuntimeInvariant::RegisterOffsetOverflow
            ))
        );
    }
}
