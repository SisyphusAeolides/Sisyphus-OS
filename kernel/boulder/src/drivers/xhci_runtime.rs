//! Halted-controller xHCI ring programming and polled command preparation.
//!
//! This is the narrow bridge between retained reset-ready evidence and a
//! future one-shot runtime. It programs only while HCHalted is proven and
//! never treats missing DMAR data as an identity-DMA witness. Bus mastering,
//! Run/Stop, and teardown remain separate transactions with their own debt.

use super::xhci::{XhciRuntimeEvidence, XhciRuntimeSeed};
use super::xhci_dma::{XhciDmaPurpose, XhciDmaQuiescence, XhciDmaRegionPhase, XhciDmaRegionRecord};
use super::xhci_iommu::{XhciIommuBindFailure, XhciIommuBinding, bind_core_regions};
use super::xhci_ring::{
    XhciCommandCompletionEvidence, XhciNoOpCommandReceipt, XhciRingError, XhciRingMachine,
    XhciRingStorage,
};

const USBCMD_RUN_STOP: u32 = 1 << 0;
const USBCMD_HOST_CONTROLLER_RESET: u32 = 1 << 1;
const USBSTS_HCHALTED: u32 = 1 << 0;
const USBSTS_HOST_CONTROLLER_ERROR: u32 = 1 << 2;
const USBSTS_CONTROLLER_NOT_READY: u32 = 1 << 11;
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
    InvalidPollLimit,
    ControllerNotHalted,
    ControllerNotRunning,
    ControllerError,
    ControllerStartTimeout,
    ControllerHaltTimeout,
    ControllerResetTimeout,
    ControllerReadyTimeout,
    ControllerResetNotEligible,
    ControllerResetFailed,
    RunningReceiptMismatch,
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

/// Evidence that a controller left the halted state while its caller retained
/// the DMA and bus-master authorities needed for the epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XhciRunningReceipt {
    runtime_root: u64,
    start_polls: u32,
    root: u64,
}

impl XhciRunningReceipt {
    pub const fn start_polls(&self) -> u32 {
        self.start_polls
    }

    pub const fn root(&self) -> u64 {
        self.root
    }
}

/// Evidence that one exact running controller returned to HCHalted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XhciRunStopReceipt {
    pub start_polls: u32,
    pub halt_polls: u32,
    pub root: u64,
}

/// Evidence that a stopped, scrubbed controller completed one host-controller
/// reset and returned to the ready, halted state.  Reset is deliberately a
/// separate linear state: no pre-reset DMA quiescence proof can release an
/// arena until the controller has stopped referring to its former runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XhciControllerResetReceipt {
    pub reset_polls: u32,
    pub ready_polls: u32,
    pub root: u64,
}

/// A prepared controller whose Run/Stop transition has been observed live.
///
/// The running receipt remains private so command publication cannot be
/// detached from the exact prepared runtime that produced it.
pub struct XhciRunningRuntime {
    prepared: XhciPreparedRuntime,
    running: XhciRunningReceipt,
}

/// A controller that has returned to HCHalted after one owned runtime epoch.
pub struct XhciHaltedRuntime {
    prepared: XhciPreparedRuntime,
    receipt: XhciRunStopReceipt,
}

/// A halted runtime whose address-bearing registers have been cleared and
/// revalidated. This is the sole runtime path that may mint DMA quiescence.
pub struct XhciScrubbedRuntime {
    prepared: XhciPreparedRuntime,
    receipt: XhciRunStopReceipt,
}

/// A scrubbed runtime after its controller has been reset and re-observed
/// ready and halted. This is the strongest terminal controller state and may
/// mint the DMA-release proof.
pub struct XhciResetRecoveredRuntime {
    prepared: XhciPreparedRuntime,
    run_stop: XhciRunStopReceipt,
    reset: XhciControllerResetReceipt,
}

/// Start failure retaining the prepared controller for explicit containment.
pub struct XhciRuntimeStartFailure<RegisterError> {
    runtime: XhciPreparedRuntime,
    cause: XhciRuntimeError<(), RegisterError>,
}

impl<RegisterError> XhciRuntimeStartFailure<RegisterError> {
    pub fn cause(&self) -> &XhciRuntimeError<(), RegisterError> {
        &self.cause
    }

    pub fn into_runtime(self) -> XhciPreparedRuntime {
        self.runtime
    }
}

/// Halt failure retaining the still-running controller and its receipt.
pub struct XhciRuntimeHaltFailure<RegisterError> {
    runtime: XhciRunningRuntime,
    cause: XhciRuntimeError<(), RegisterError>,
}

impl<RegisterError> XhciRuntimeHaltFailure<RegisterError> {
    pub fn cause(&self) -> &XhciRuntimeError<(), RegisterError> {
        &self.cause
    }

    pub fn into_runtime(self) -> XhciRunningRuntime {
        self.runtime
    }
}

/// Scrub failure retaining halted register provenance and the halt receipt.
pub struct XhciRuntimeScrubFailure<RegisterError> {
    runtime: XhciHaltedRuntime,
    cause: XhciRuntimeError<(), RegisterError>,
}

/// Reset failure retaining the already-scrubbed runtime.  The caller still
/// owns all DMA and PCI authorities and must not release either underneath an
/// unobserved controller state.
pub struct XhciRuntimeResetFailure<RegisterError> {
    runtime: XhciScrubbedRuntime,
    cause: XhciRuntimeError<(), RegisterError>,
}

impl<RegisterError> XhciRuntimeResetFailure<RegisterError> {
    pub fn cause(&self) -> &XhciRuntimeError<(), RegisterError> {
        &self.cause
    }

    pub fn into_runtime(self) -> XhciScrubbedRuntime {
        self.runtime
    }
}

impl<RegisterError> XhciRuntimeScrubFailure<RegisterError> {
    pub fn cause(&self) -> &XhciRuntimeError<(), RegisterError> {
        &self.cause
    }

    pub fn into_runtime(self) -> XhciHaltedRuntime {
        self.runtime
    }
}

/// The result of programming the controller's address-bearing ring registers
/// while it is still halted. No command has been submitted and no bus-master
/// bit has been enabled by this object.
pub struct XhciPreparedRuntime {
    rings: XhciRingMachine,
    device: crate::hw::pci::PciAddress,
    generation: u32,
    reset_ready_root: u64,
    operational_offset: u32,
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
        running: &XhciRunningReceipt,
        storage: &S,
        registers: &mut R,
    ) -> Result<XhciNoOpCommandReceipt, XhciRuntimeError<S::Error, R::Error>>
    where
        S: XhciRingStorage,
        R: XhciRuntimeRegisters,
    {
        self.validate_running_receipt(running)?;
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
        running: &XhciRunningReceipt,
        storage: &S,
        registers: &mut R,
        receipt: &XhciNoOpCommandReceipt,
    ) -> Result<Option<XhciCommandCompletionEvidence>, XhciRuntimeError<S::Error, R::Error>>
    where
        S: XhciRingStorage,
        R: XhciRuntimeRegisters,
    {
        self.validate_running_receipt(running)?;
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

    /// Removes every address-bearing runtime register while HCHalted remains
    /// observed.  This is the reverse half of a reversible preparation epoch:
    /// after it succeeds, the backing arena may be reclaimed without leaving
    /// stale physical addresses armed for a later controller start.
    pub fn scrub_halted<R>(&self, registers: &mut R) -> Result<(), XhciRuntimeError<(), R::Error>>
    where
        R: XhciRuntimeRegisters,
    {
        scrub_halted_registers(registers, self.operational_offset, self.runtime_offset)
    }

    /// Starts the controller after translated DMA and PCI bus-master authority
    /// have both been installed by the caller.
    pub fn start<R>(
        &self,
        registers: &mut R,
        poll_limit: u32,
    ) -> Result<XhciRunningReceipt, XhciRuntimeError<(), R::Error>>
    where
        R: XhciRuntimeRegisters,
    {
        start_registers(
            registers,
            self.operational_offset,
            self.runtime_root,
            poll_limit,
        )
    }

    /// Consumes halted preparation into the only state that may publish
    /// polled commands. If start is not observed, the prepared runtime is
    /// returned to the caller rather than silently discarded.
    pub fn start_session<R>(
        self,
        registers: &mut R,
        poll_limit: u32,
    ) -> Result<XhciRunningRuntime, XhciRuntimeStartFailure<R::Error>>
    where
        R: XhciRuntimeRegisters,
    {
        match self.start(registers, poll_limit) {
            Ok(running) => Ok(XhciRunningRuntime {
                prepared: self,
                running,
            }),
            Err(cause) => Err(XhciRuntimeStartFailure {
                runtime: self,
                cause,
            }),
        }
    }

    /// Stops the exact controller named by a running receipt. The caller must
    /// retain DMA and bus-master authority until this returns successfully.
    pub fn halt<R>(
        &self,
        running: XhciRunningReceipt,
        registers: &mut R,
        poll_limit: u32,
    ) -> Result<XhciRunStopReceipt, XhciRuntimeError<(), R::Error>>
    where
        R: XhciRuntimeRegisters,
    {
        if running.runtime_root != self.runtime_root {
            return Err(XhciRuntimeError::Invariant(
                XhciRuntimeInvariant::RunningReceiptMismatch,
            ));
        }
        halt_registers(
            registers,
            self.operational_offset,
            self.runtime_root,
            running,
            poll_limit,
        )
    }

    /// Performs one bounded Run/Stop transition without publishing commands.
    /// It is a reversible controller transition probe for the future session
    /// owner; it neither enables bus mastering nor releases DMA receipts.
    pub fn run_then_halt<R>(
        &self,
        registers: &mut R,
        poll_limit: u32,
    ) -> Result<XhciRunStopReceipt, XhciRuntimeError<(), R::Error>>
    where
        R: XhciRuntimeRegisters,
    {
        let running = self.start(registers, poll_limit)?;
        self.halt(running, registers, poll_limit)
    }

    fn event_dequeue_pointer_offset(&self) -> u32 {
        self.runtime_offset + RUNTIME_INTERRUPTER + RT_ERDP
    }

    fn validate_running_receipt<StorageError, RegisterError>(
        &self,
        running: &XhciRunningReceipt,
    ) -> Result<(), XhciRuntimeError<StorageError, RegisterError>> {
        if running.runtime_root != self.runtime_root {
            return Err(XhciRuntimeError::Invariant(
                XhciRuntimeInvariant::RunningReceiptMismatch,
            ));
        }
        Ok(())
    }
}

impl XhciRunningRuntime {
    pub const fn runtime_root(&self) -> u64 {
        self.prepared.runtime_root()
    }

    pub const fn running_root(&self) -> u64 {
        self.running.root()
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
        self.prepared
            .submit_no_op(&self.running, storage, registers)
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
        self.prepared
            .poll_no_op_completion(&self.running, storage, registers, receipt)
    }

    /// Stops this exact session. A failure retains the entire running state,
    /// so DMA and bus-master owners cannot continue teardown by accident.
    pub fn halt<R>(
        self,
        registers: &mut R,
        poll_limit: u32,
    ) -> Result<XhciHaltedRuntime, XhciRuntimeHaltFailure<R::Error>>
    where
        R: XhciRuntimeRegisters,
    {
        let Self { prepared, running } = self;
        match prepared.halt(running, registers, poll_limit) {
            Ok(receipt) => Ok(XhciHaltedRuntime { prepared, receipt }),
            Err(cause) => Err(XhciRuntimeHaltFailure {
                runtime: Self { prepared, running },
                cause,
            }),
        }
    }
}

impl XhciHaltedRuntime {
    pub const fn halt_receipt(&self) -> XhciRunStopReceipt {
        self.receipt
    }

    pub const fn runtime_root(&self) -> u64 {
        self.prepared.runtime_root()
    }

    /// Scrubs all DMA-bearing registers and returns a state that can mint an
    /// arena-release proof only after HCHalted is re-observed.
    pub fn scrub<R>(
        self,
        registers: &mut R,
    ) -> Result<XhciScrubbedRuntime, XhciRuntimeScrubFailure<R::Error>>
    where
        R: XhciRuntimeRegisters,
    {
        let Self { prepared, receipt } = self;
        match prepared.scrub_halted(registers) {
            Ok(()) => Ok(XhciScrubbedRuntime { prepared, receipt }),
            Err(cause) => Err(XhciRuntimeScrubFailure {
                runtime: Self { prepared, receipt },
                cause,
            }),
        }
    }
}

impl XhciScrubbedRuntime {
    pub const fn runtime_root(&self) -> u64 {
        self.prepared.runtime_root()
    }

    pub const fn halt_receipt(&self) -> XhciRunStopReceipt {
        self.receipt
    }

    /// Resets a stopped controller after its runtime registers are scrubbed.
    /// The initial state may carry Host System Error: a reset is precisely the
    /// recovery action for that state. Completion still requires HSE clear,
    /// CNR clear, and HCHalted set before DMA can be reclaimed.
    pub fn reset_controller<R>(
        self,
        registers: &mut R,
        poll_limit: u32,
    ) -> Result<XhciResetRecoveredRuntime, XhciRuntimeResetFailure<R::Error>>
    where
        R: XhciRuntimeRegisters,
    {
        let Self { prepared, receipt } = self;
        match reset_halted_controller(
            registers,
            prepared.operational_offset,
            prepared.runtime_root,
            receipt,
            poll_limit,
        ) {
            Ok(reset) => Ok(XhciResetRecoveredRuntime {
                prepared,
                run_stop: receipt,
                reset,
            }),
            Err(cause) => Err(XhciRuntimeResetFailure {
                runtime: Self { prepared, receipt },
                cause,
            }),
        }
    }

    fn into_dma_quiescence_inner(self) -> XhciDmaQuiescence {
        // SAFETY: construction requires an observed HCHalted receipt, and
        // `scrub` re-observes HCHalted only after clearing every address-
        // bearing xHCI runtime register. The caller retains PCI bus-master
        // ownership until this value is consumed by arena release.
        unsafe {
            XhciDmaQuiescence::establish(
                self.prepared.device,
                self.prepared.generation,
                self.prepared.reset_ready_root,
            )
        }
    }
}

impl XhciResetRecoveredRuntime {
    pub const fn runtime_root(&self) -> u64 {
        self.prepared.runtime_root()
    }

    pub const fn run_stop_receipt(&self) -> XhciRunStopReceipt {
        self.run_stop
    }

    pub const fn reset_receipt(&self) -> XhciControllerResetReceipt {
        self.reset
    }

    /// Converts reset-completion evidence into the sole DMA-release proof.
    /// This consumes the stronger post-reset state, so a caller cannot claim
    /// that a reset succeeded while releasing a pre-reset runtime instead.
    pub fn into_dma_quiescence(self) -> XhciDmaQuiescence {
        let Self {
            prepared,
            run_stop,
            reset: _,
        } = self;
        XhciScrubbedRuntime {
            prepared,
            receipt: run_stop,
        }
        .into_dma_quiescence_inner()
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
    prepare_halted_from_evidence(
        seed.evidence(),
        seed.aperture().length(),
        storage,
        registers,
        secret,
    )
}

/// Programs a halted controller from revalidated retained evidence without
/// consuming the reset-ready controller.  It exists for reversible hardware
/// preparation; an operational epoch must still consume `XhciRuntimeSeed`.
pub fn prepare_halted_from_evidence<S, R>(
    evidence: XhciRuntimeEvidence,
    aperture_length: u64,
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
    let operational_offset = u32::from(evidence.snapshot.capability_length);
    verify_halted::<S::Error, _>(registers, operational_offset)?;

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
    if u64::from(runtime_end) > aperture_length {
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
    if u64::from(doorbell_end) > aperture_length {
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
        device: evidence.address,
        generation: evidence.generation,
        reset_ready_root: evidence.reset_ready_root,
        operational_offset,
        runtime_offset,
        doorbell_offset: doorbell,
        event_dequeue_pointer: program.event_ring_dequeue_pointer,
        runtime_root: canonical_root(root),
    })
}

fn verify_halted<StorageError, R>(
    registers: &mut R,
    operational_offset: u32,
) -> Result<(), XhciRuntimeError<StorageError, R::Error>>
where
    R: XhciRuntimeRegisters,
{
    verify_stopped(registers, operational_offset)?;
    let status_offset = checked_offset(operational_offset, 4)?;
    let status = registers
        .read32(status_offset)
        .map_err(XhciRuntimeError::Register)?;
    if status & USBSTS_HOST_CONTROLLER_ERROR != 0 {
        return Err(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::ControllerError,
        ));
    }
    Ok(())
}

/// HCHalted is sufficient to clear stale runtime register references even
/// when USBSTS.HSE is set. The following reset transaction is responsible for
/// clearing HSE before an arena-release proof is minted.
fn verify_stopped<StorageError, R>(
    registers: &mut R,
    operational_offset: u32,
) -> Result<(), XhciRuntimeError<StorageError, R::Error>>
where
    R: XhciRuntimeRegisters,
{
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
    Ok(())
}

fn scrub_halted_registers<R>(
    registers: &mut R,
    operational_offset: u32,
    runtime_offset: u32,
) -> Result<(), XhciRuntimeError<(), R::Error>>
where
    R: XhciRuntimeRegisters,
{
    verify_stopped(registers, operational_offset)?;
    let crcr = checked_offset(operational_offset, OP_CRCR)?;
    let dcbaap = checked_offset(operational_offset, OP_DCBAAP)?;
    let config = checked_offset(operational_offset, OP_CONFIG)?;
    let erstsz = runtime_offset
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
    registers
        .write64(erdp, 0)
        .map_err(XhciRuntimeError::Register)?;
    registers
        .write64(erstba, 0)
        .map_err(XhciRuntimeError::Register)?;
    registers
        .write32(erstsz, 0)
        .map_err(XhciRuntimeError::Register)?;
    registers
        .write64(crcr, 0)
        .map_err(XhciRuntimeError::Register)?;
    registers
        .write64(dcbaap, 0)
        .map_err(XhciRuntimeError::Register)?;
    registers
        .write32(config, 0)
        .map_err(XhciRuntimeError::Register)?;
    verify_stopped(registers, operational_offset)
}

fn start_registers<R>(
    registers: &mut R,
    operational_offset: u32,
    runtime_root: u64,
    poll_limit: u32,
) -> Result<XhciRunningReceipt, XhciRuntimeError<(), R::Error>>
where
    R: XhciRuntimeRegisters,
{
    if poll_limit == 0 {
        return Err(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::InvalidPollLimit,
        ));
    }
    verify_halted(registers, operational_offset)?;
    let command = registers
        .read32(operational_offset)
        .map_err(XhciRuntimeError::Register)?;
    let status_offset = checked_offset(operational_offset, 4)?;
    registers
        .write32(operational_offset, command | USBCMD_RUN_STOP)
        .map_err(XhciRuntimeError::Register)?;

    let start_polls = match wait_for_halted(registers, status_offset, false, poll_limit) {
        Ok(polls) => polls,
        Err(error) => {
            // A rejected start must still receive an explicit stop request.
            // The caller retains all authority if containment cannot be
            // observed, preventing arena or domain release underneath DMA.
            let _ = registers.write32(operational_offset, command & !USBCMD_RUN_STOP);
            let _ = wait_for_halted(registers, status_offset, true, poll_limit);
            return Err(error);
        }
    };
    let mut root = mix(runtime_root, u64::from(start_polls));
    root = mix(root, u64::from(command));
    Ok(XhciRunningReceipt {
        runtime_root,
        start_polls,
        root: canonical_root(root),
    })
}

fn halt_registers<R>(
    registers: &mut R,
    operational_offset: u32,
    runtime_root: u64,
    running: XhciRunningReceipt,
    poll_limit: u32,
) -> Result<XhciRunStopReceipt, XhciRuntimeError<(), R::Error>>
where
    R: XhciRuntimeRegisters,
{
    if poll_limit == 0 {
        return Err(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::InvalidPollLimit,
        ));
    }
    let command = registers
        .read32(operational_offset)
        .map_err(XhciRuntimeError::Register)?;
    if command & USBCMD_RUN_STOP == 0 {
        return Err(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::ControllerNotRunning,
        ));
    }
    let status_offset = checked_offset(operational_offset, 4)?;
    registers
        .write32(operational_offset, command & !USBCMD_RUN_STOP)
        .map_err(XhciRuntimeError::Register)?;
    let halt_polls = wait_for_halted(registers, status_offset, true, poll_limit)?;
    let mut root = mix(runtime_root, u64::from(running.start_polls));
    root = mix(root, u64::from(halt_polls));
    root = mix(root, u64::from(command));
    Ok(XhciRunStopReceipt {
        start_polls: running.start_polls,
        halt_polls,
        root: canonical_root(root),
    })
}

fn reset_halted_controller<R>(
    registers: &mut R,
    operational_offset: u32,
    runtime_root: u64,
    run_stop: XhciRunStopReceipt,
    poll_limit: u32,
) -> Result<XhciControllerResetReceipt, XhciRuntimeError<(), R::Error>>
where
    R: XhciRuntimeRegisters,
{
    if poll_limit == 0 {
        return Err(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::InvalidPollLimit,
        ));
    }
    let status_offset = checked_offset(operational_offset, 4)?;
    let command = registers
        .read32(operational_offset)
        .map_err(XhciRuntimeError::Register)?;
    let status = registers
        .read32(status_offset)
        .map_err(XhciRuntimeError::Register)?;
    if command & (USBCMD_RUN_STOP | USBCMD_HOST_CONTROLLER_RESET) != 0
        || status & (USBSTS_HCHALTED | USBSTS_CONTROLLER_NOT_READY) != USBSTS_HCHALTED
    {
        return Err(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::ControllerResetNotEligible,
        ));
    }

    registers
        .write32(operational_offset, command | USBCMD_HOST_CONTROLLER_RESET)
        .map_err(XhciRuntimeError::Register)?;
    let reset_polls = wait_for_command_bit(
        registers,
        operational_offset,
        USBCMD_HOST_CONTROLLER_RESET,
        false,
        poll_limit,
        XhciRuntimeInvariant::ControllerResetTimeout,
    )?;
    let ready_polls = wait_for_status_ready(registers, status_offset, poll_limit)?;

    let settled_command = registers
        .read32(operational_offset)
        .map_err(XhciRuntimeError::Register)?;
    let settled_status = registers
        .read32(status_offset)
        .map_err(XhciRuntimeError::Register)?;
    if settled_command & (USBCMD_RUN_STOP | USBCMD_HOST_CONTROLLER_RESET) != 0
        || settled_status & (USBSTS_HCHALTED | USBSTS_CONTROLLER_NOT_READY) != USBSTS_HCHALTED
        || settled_status & USBSTS_HOST_CONTROLLER_ERROR != 0
    {
        return Err(XhciRuntimeError::Invariant(
            XhciRuntimeInvariant::ControllerResetFailed,
        ));
    }

    let mut root = mix(runtime_root, run_stop.root);
    root = mix(root, u64::from(reset_polls));
    root = mix(root, u64::from(ready_polls));
    Ok(XhciControllerResetReceipt {
        reset_polls,
        ready_polls,
        root: canonical_root(root),
    })
}

fn wait_for_command_bit<R>(
    registers: &mut R,
    offset: u32,
    bit: u32,
    expected_set: bool,
    poll_limit: u32,
    timeout: XhciRuntimeInvariant,
) -> Result<u32, XhciRuntimeError<(), R::Error>>
where
    R: XhciRuntimeRegisters,
{
    for polls in 1..=poll_limit {
        let command = registers
            .read32(offset)
            .map_err(XhciRuntimeError::Register)?;
        if (command & bit != 0) == expected_set {
            return Ok(polls);
        }
    }
    Err(XhciRuntimeError::Invariant(timeout))
}

fn wait_for_status_ready<R>(
    registers: &mut R,
    status_offset: u32,
    poll_limit: u32,
) -> Result<u32, XhciRuntimeError<(), R::Error>>
where
    R: XhciRuntimeRegisters,
{
    for polls in 1..=poll_limit {
        let status = registers
            .read32(status_offset)
            .map_err(XhciRuntimeError::Register)?;
        if status & USBSTS_CONTROLLER_NOT_READY == 0 {
            return Ok(polls);
        }
    }
    Err(XhciRuntimeError::Invariant(
        XhciRuntimeInvariant::ControllerReadyTimeout,
    ))
}

fn wait_for_halted<R>(
    registers: &mut R,
    status_offset: u32,
    expected_halted: bool,
    poll_limit: u32,
) -> Result<u32, XhciRuntimeError<(), R::Error>>
where
    R: XhciRuntimeRegisters,
{
    for polls in 1..=poll_limit {
        let status = registers
            .read32(status_offset)
            .map_err(XhciRuntimeError::Register)?;
        if status & USBSTS_HOST_CONTROLLER_ERROR != 0 {
            return Err(XhciRuntimeError::Invariant(
                XhciRuntimeInvariant::ControllerError,
            ));
        }
        if (status & USBSTS_HCHALTED != 0) == expected_halted {
            return Ok(polls);
        }
    }
    Err(XhciRuntimeError::Invariant(if expected_halted {
        XhciRuntimeInvariant::ControllerHaltTimeout
    } else {
        XhciRuntimeInvariant::ControllerStartTimeout
    }))
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
        status_offset: u32,
        last_offset: u32,
        last_value: u64,
        writes: u8,
    }

    impl XhciRuntimeRegisters for RegisterProbe {
        type Error = ();

        fn read32(&mut self, offset: u32) -> Result<u32, Self::Error> {
            Ok((offset == self.status_offset)
                .then_some(self.read_value)
                .unwrap_or(0))
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

    #[test]
    fn halted_scrub_zeros_all_address_bearing_runtime_registers() {
        let mut probe = RegisterProbe {
            read_value: USBSTS_HCHALTED,
            status_offset: 0x24,
            ..RegisterProbe::default()
        };
        scrub_halted_registers(&mut probe, 0x20, 0x100).unwrap();
        assert_eq!(probe.writes, 6);
        assert_eq!(probe.last_offset, 0x58);
        assert_eq!(probe.last_value, 0);
    }

    struct RunStopProbe {
        command: u32,
        status_offset: u32,
        host_error: bool,
        controller_not_ready: bool,
        reset_stuck: bool,
        clear_error_on_reset: bool,
        clear_ready_on_reset: bool,
    }

    impl XhciRuntimeRegisters for RunStopProbe {
        type Error = ();

        fn read32(&mut self, offset: u32) -> Result<u32, Self::Error> {
            if offset == self.status_offset {
                let halted = (self.command & USBCMD_RUN_STOP == 0) as u32 * USBSTS_HCHALTED;
                return Ok(halted
                    | (self.host_error as u32 * USBSTS_HOST_CONTROLLER_ERROR)
                    | (self.controller_not_ready as u32 * USBSTS_CONTROLLER_NOT_READY));
            }
            Ok(self.command)
        }

        fn write32(&mut self, _offset: u32, value: u32) -> Result<(), Self::Error> {
            self.command = if value & USBCMD_HOST_CONTROLLER_RESET != 0 && !self.reset_stuck {
                if self.clear_error_on_reset {
                    self.host_error = false;
                }
                if self.clear_ready_on_reset {
                    self.controller_not_ready = false;
                }
                value & !USBCMD_HOST_CONTROLLER_RESET
            } else {
                value
            };
            Ok(())
        }

        fn write64(&mut self, _offset: u32, _value: u64) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[test]
    fn bounded_run_stop_receipt_requires_observed_start_and_halt() {
        let mut probe = RunStopProbe {
            command: 0,
            status_offset: 0x24,
            host_error: false,
            controller_not_ready: false,
            reset_stuck: false,
            clear_error_on_reset: false,
            clear_ready_on_reset: false,
        };
        let running = start_registers(&mut probe, 0x20, 0x5eed, 4).unwrap();
        assert_eq!(running.start_polls(), 1);
        let receipt = halt_registers(&mut probe, 0x20, 0x5eed, running, 4).unwrap();
        assert_eq!(receipt.start_polls, 1);
        assert_eq!(receipt.halt_polls, 1);
        assert_ne!(receipt.root, 0);
        assert_eq!(probe.command & USBCMD_RUN_STOP, 0);
    }

    #[test]
    fn run_stop_rejects_host_error_before_start() {
        let mut probe = RunStopProbe {
            command: 0,
            status_offset: 0x24,
            host_error: true,
            controller_not_ready: false,
            reset_stuck: false,
            clear_error_on_reset: false,
            clear_ready_on_reset: false,
        };
        assert_eq!(
            start_registers(&mut probe, 0x20, 0x5eed, 4),
            Err(XhciRuntimeError::Invariant(
                XhciRuntimeInvariant::ControllerError
            ))
        );
        assert_eq!(probe.command & USBCMD_RUN_STOP, 0);
    }

    #[test]
    fn reset_recovery_accepts_halted_host_error_and_requires_it_to_clear() {
        let mut probe = RunStopProbe {
            command: 0,
            status_offset: 0x24,
            host_error: true,
            controller_not_ready: false,
            reset_stuck: false,
            clear_error_on_reset: true,
            clear_ready_on_reset: true,
        };
        let receipt = reset_halted_controller(
            &mut probe,
            0x20,
            0x5eed,
            XhciRunStopReceipt {
                start_polls: 1,
                halt_polls: 1,
                root: 0x1234,
            },
            4,
        )
        .unwrap();
        assert_eq!(receipt.reset_polls, 1);
        assert_eq!(receipt.ready_polls, 1);
        assert_ne!(receipt.root, 0);
        assert!(!probe.host_error);
        assert_eq!(probe.command & USBCMD_HOST_CONTROLLER_RESET, 0);
    }

    #[test]
    fn scrub_retains_halted_containment_when_host_error_requires_reset() {
        let mut probe = RunStopProbe {
            command: 0,
            status_offset: 0x24,
            host_error: true,
            controller_not_ready: false,
            reset_stuck: false,
            clear_error_on_reset: false,
            clear_ready_on_reset: false,
        };
        scrub_halted_registers(&mut probe, 0x20, 0x100).unwrap();
        assert!(probe.host_error);
        assert_eq!(probe.command & USBCMD_RUN_STOP, 0);
    }

    #[test]
    fn reset_recovery_retains_failure_when_reset_bit_never_clears() {
        let mut probe = RunStopProbe {
            command: 0,
            status_offset: 0x24,
            host_error: true,
            controller_not_ready: false,
            reset_stuck: true,
            clear_error_on_reset: false,
            clear_ready_on_reset: false,
        };
        assert_eq!(
            reset_halted_controller(
                &mut probe,
                0x20,
                0x5eed,
                XhciRunStopReceipt {
                    start_polls: 1,
                    halt_polls: 1,
                    root: 0x1234,
                },
                2,
            ),
            Err(XhciRuntimeError::Invariant(
                XhciRuntimeInvariant::ControllerResetTimeout
            ))
        );
    }
}
