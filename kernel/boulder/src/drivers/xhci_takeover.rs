//! Bounded xHCI firmware handoff, halt, and reset transaction.
//!
//! The machine deliberately performs at most one register/deadline batch per
//! [`TakeoverMachine::step`].  It never spins internally, so the caller can
//! interleave watchdog, containment, and audit work while retaining an exact
//! phase boundary for every controller mutation.

use crate::interrupts::{
    DeadlineClock, DeadlineLease, DeadlineState, LocalApicDeadlineClock, TimerError,
};
use crate::mmio::{MmioAccessError, MmioWindow};

pub const FIRMWARE_HANDOFF_DEADLINE_NS: u64 = 1_000_000_000;
pub const INITIAL_READY_DEADLINE_NS: u64 = 1_000_000_000;
pub const USB2_RESET_DRAIN_DEADLINE_NS: u64 = 1_000_000_000;
pub const HALT_DEADLINE_NS: u64 = 16_000_000;
pub const RESET_DEADLINE_NS: u64 = 1_000_000_000;
pub const POST_RESET_READY_DEADLINE_NS: u64 = 1_000_000_000;
pub const MAXIMUM_USB2_PROTOCOL_RANGES: usize = 16;

const USB_COMMAND: u32 = 0x00;
const USB_STATUS: u32 = 0x04;
const PORT_REGISTER_BASE: u32 = 0x400;
const PORT_REGISTER_STRIDE: u32 = 0x10;
const LEGACY_BIOS_SEMAPHORE: u32 = 0x02;
const LEGACY_OS_SEMAPHORE: u32 = 0x03;
const LEGACY_CONTROL_STATUS: u32 = 0x04;

const COMMAND_RUN_STOP: u32 = 1 << 0;
const COMMAND_HOST_CONTROLLER_RESET: u32 = 1 << 1;
const STATUS_HOST_CONTROLLER_HALTED: u32 = 1 << 0;
const STATUS_CONTROLLER_NOT_READY: u32 = 1 << 11;
// USBSTS.HSE is bit 2. Bit 12 is reserved by xHCI and must never be used as
// a host-error witness, otherwise a genuinely faulted controller can pass
// takeover's initial-ready gate.
const STATUS_HOST_CONTROLLER_ERROR: u32 = 1 << 2;
const PORT_RESET: u32 = 1 << 4;

// These are the only writable SMI-enable fields defined by USBLEGCTLSTS.
const LEGACY_SMI_ENABLES: u32 = (1 << 0) | (1 << 4) | (1 << 13) | (1 << 14) | (1 << 15);
// Writing one to these status fields acknowledges them.  A masking write must
// write zero even when the observation returned one.
const LEGACY_RW1C_STATUS: u32 = (1 << 29) | (1 << 30) | (1 << 31);

pub trait RegisterIo {
    type Error;

    fn read8(&mut self, offset: u32) -> Result<u8, Self::Error>;
    fn write8(&mut self, offset: u32, value: u8) -> Result<(), Self::Error>;
    fn read32(&mut self, offset: u32) -> Result<u32, Self::Error>;
    fn write32(&mut self, offset: u32, value: u32) -> Result<(), Self::Error>;
}

impl RegisterIo for MmioWindow {
    type Error = MmioAccessError;

    fn read8(&mut self, offset: u32) -> Result<u8, Self::Error> {
        MmioWindow::read_u8(self, offset as usize)
    }

    fn write8(&mut self, offset: u32, value: u8) -> Result<(), Self::Error> {
        MmioWindow::write_u8(self, offset as usize, value)
    }

    fn read32(&mut self, offset: u32) -> Result<u32, Self::Error> {
        MmioWindow::read_u32(self, offset as usize)
    }

    fn write32(&mut self, offset: u32, value: u32) -> Result<(), Self::Error> {
        MmioWindow::write_u32(self, offset as usize, value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelativeDeadlineState {
    Pending,
    Expired,
}

pub trait RelativeDeadline {
    type Error;

    fn arm(&mut self, duration_ns: u64) -> Result<(), Self::Error>;
    fn poll(&mut self) -> Result<RelativeDeadlineState, Self::Error>;
    fn cancel(&mut self) -> Result<(), Self::Error>;
}

/// Adapts exclusive local-APIC deadline ownership to the takeover transaction.
pub struct ApicRelativeDeadline<'a> {
    clock: &'a mut LocalApicDeadlineClock,
    lease: Option<DeadlineLease>,
}

impl<'a> ApicRelativeDeadline<'a> {
    pub fn new(clock: &'a mut LocalApicDeadlineClock) -> Self {
        Self { clock, lease: None }
    }
}

impl RelativeDeadline for ApicRelativeDeadline<'_> {
    type Error = TimerError;

    fn arm(&mut self, duration_ns: u64) -> Result<(), Self::Error> {
        if self.lease.is_some() {
            return Err(TimerError::DeadlineBusy);
        }
        self.lease = Some(self.clock.arm(duration_ns)?);
        Ok(())
    }

    fn poll(&mut self) -> Result<RelativeDeadlineState, Self::Error> {
        let lease = self.lease.as_mut().ok_or(TimerError::StaleDeadline)?;
        match self.clock.poll(lease)? {
            DeadlineState::Pending => Ok(RelativeDeadlineState::Pending),
            DeadlineState::Expired => {
                self.lease = None;
                Ok(RelativeDeadlineState::Expired)
            }
        }
    }

    fn cancel(&mut self) -> Result<(), Self::Error> {
        let lease = self.lease.take().ok_or(TimerError::StaleDeadline)?;
        self.clock.cancel(lease)
    }
}

impl Drop for ApicRelativeDeadline<'_> {
    fn drop(&mut self) {
        if let Some(lease) = self.lease.take() {
            let _ = self.clock.cancel(lease);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Usb2ProtocolPorts {
    /// xHCI port identifiers are one-based.
    pub first_port: u8,
    pub port_count: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TakeoverConfig<'a> {
    /// Provisional mapping boundary used only to contain the pre-sizing
    /// firmware-handoff and halt transaction. This is not a BAR-size claim.
    pub bootstrap_containment_bytes: u64,
    pub operational_offset: u32,
    pub legacy_support_offset: Option<u32>,
    pub maximum_ports: u8,
    /// USB 2.x Supported Protocol ranges obtained from already-retained
    /// evidence. Empty deliberately falls back to scanning every MaxPorts
    /// PortSC register before halt; it never assumes an unknown port is idle.
    pub usb2_protocols: &'a [Usb2ProtocolPorts],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TakeoverConfigError {
    BootstrapContainmentTooSmall,
    MisalignedOperationalOffset,
    InvalidLegacyOffset,
    InvalidMaximumPorts,
    TooManyUsb2ProtocolRanges,
    InvalidUsb2ProtocolRange,
    OverlappingUsb2ProtocolRange,
    OffsetOverflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TakeoverPhase {
    ClaimLegacyOwnership,
    ArmLegacyHandoff,
    AwaitLegacyHandoff,
    MaskLegacySmi,
    VerifyLegacySmiMasked,
    ArmInitialReady,
    AwaitInitialReady,
    ArmUsb2ResetDrain,
    DrainUsb2Resets,
    RequestHalt,
    ArmHalt,
    AwaitHalt,
    AwaitMeasuredAperture,
    RequestReset,
    ArmReset,
    AwaitReset,
    ArmPostResetReady,
    AwaitPostResetReady,
    VerifyReadyHalted,
    ReadyHalted,
    Faulted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegisterOperation {
    Read8,
    Write8,
    Read32,
    Write32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeadlineOperation {
    Arm,
    Poll,
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitClass {
    FirmwareHandoff,
    InitialControllerReady,
    Usb2PortResetDrain,
    ControllerHalt,
    ControllerReset,
    PostResetControllerReady,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TakeoverFaultClass {
    RegisterAccess,
    Deadline,
    Timeout,
    HostControllerError,
    IllegalControllerState,
    ReadyHaltedInvariant,
    MeasuredApertureRequired,
    TerminalState,
}

#[derive(Debug, Eq, PartialEq)]
pub enum TakeoverFault<RegisterError, DeadlineError> {
    Register {
        phase: TakeoverPhase,
        operation: RegisterOperation,
        offset: u32,
        source: RegisterError,
    },
    Deadline {
        phase: TakeoverPhase,
        operation: DeadlineOperation,
        source: DeadlineError,
    },
    Timeout(WaitClass),
    LegacySmiMaskRejected {
        readback: u32,
    },
    HostControllerError {
        phase: TakeoverPhase,
        status: u32,
    },
    IllegalControllerState {
        phase: TakeoverPhase,
        command: u32,
        status: u32,
    },
    ReadyHaltedInvariant {
        command: u32,
        status: u32,
    },
    IllegalTransition(TakeoverPhase),
    MeasuredApertureRequired,
    TerminalState(TakeoverPhase),
}

impl<RegisterError, DeadlineError> TakeoverFault<RegisterError, DeadlineError> {
    pub const fn class(&self) -> TakeoverFaultClass {
        match self {
            Self::Register { .. } => TakeoverFaultClass::RegisterAccess,
            Self::Deadline { .. } => TakeoverFaultClass::Deadline,
            Self::Timeout(_) => TakeoverFaultClass::Timeout,
            Self::LegacySmiMaskRejected { .. } => TakeoverFaultClass::IllegalControllerState,
            Self::HostControllerError { .. } => TakeoverFaultClass::HostControllerError,
            Self::IllegalControllerState { .. } => TakeoverFaultClass::IllegalControllerState,
            Self::ReadyHaltedInvariant { .. } => TakeoverFaultClass::ReadyHaltedInvariant,
            Self::IllegalTransition(_) => TakeoverFaultClass::IllegalControllerState,
            Self::MeasuredApertureRequired => TakeoverFaultClass::MeasuredApertureRequired,
            Self::TerminalState(_) => TakeoverFaultClass::TerminalState,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct ReadyHalted {
    command: u32,
    status: u32,
    measured_aperture_bytes: u64,
    legacy_handoff_performed: bool,
    ports_observed: u16,
}

impl ReadyHalted {
    pub const fn command(&self) -> u32 {
        self.command
    }

    pub const fn status(&self) -> u32 {
        self.status
    }

    pub const fn measured_aperture_bytes(&self) -> u64 {
        self.measured_aperture_bytes
    }

    pub const fn legacy_handoff_performed(&self) -> bool {
        self.legacy_handoff_performed
    }

    pub const fn ports_observed(&self) -> u16 {
        self.ports_observed
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum TakeoverObservation {
    Transition {
        completed: TakeoverPhase,
        next: TakeoverPhase,
    },
    LegacyOwnership {
        bios_owned: bool,
        os_owned: bool,
        next: TakeoverPhase,
    },
    LegacySmiMasked {
        observed: u32,
        written: u32,
        next: TakeoverPhase,
    },
    LegacySmiVerified {
        readback: u32,
        next: TakeoverPhase,
    },
    ControllerStatus {
        completed: TakeoverPhase,
        status: u32,
        next: TakeoverPhase,
    },
    PortResetDrain {
        port_id: u8,
        port_status: u32,
        usb2_protocol_evidenced: bool,
        next: TakeoverPhase,
    },
    ControllerCommand {
        completed: TakeoverPhase,
        observed: u32,
        written: u32,
        next: TakeoverPhase,
    },
    Ready(ReadyHalted),
}

pub struct TakeoverMachine<'a> {
    config: TakeoverConfig<'a>,
    phase: TakeoverPhase,
    usb2_range_index: usize,
    usb2_port_in_range: u8,
    usb2_cycle_saw_reset: bool,
    ports_observed: u16,
    measured_aperture_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApertureBindingError {
    WrongPhase(TakeoverPhase),
    ZeroLength,
    NotPowerOfTwo,
    ExceedsBootstrapContainment,
    DoesNotContainTransactionRegisters,
}

impl<'a> TakeoverMachine<'a> {
    pub fn new(config: TakeoverConfig<'a>) -> Result<Self, TakeoverConfigError> {
        validate_config(config)?;
        Ok(Self {
            phase: if config.legacy_support_offset.is_some() {
                TakeoverPhase::ClaimLegacyOwnership
            } else {
                TakeoverPhase::ArmInitialReady
            },
            config,
            usb2_range_index: 0,
            usb2_port_in_range: 0,
            usb2_cycle_saw_reset: false,
            ports_observed: 0,
            measured_aperture_bytes: None,
        })
    }

    pub const fn phase(&self) -> TakeoverPhase {
        self.phase
    }

    pub const fn is_ready_halted(&self) -> bool {
        matches!(self.phase, TakeoverPhase::ReadyHalted)
    }

    /// Binds the exact BAR aperture measured while the controller is OS-owned
    /// and halted. Reset authority does not exist before this succeeds.
    pub fn bind_measured_aperture(
        &mut self,
        aperture_bytes: u64,
    ) -> Result<(), ApertureBindingError> {
        if self.phase != TakeoverPhase::AwaitMeasuredAperture {
            return Err(ApertureBindingError::WrongPhase(self.phase));
        }
        if aperture_bytes == 0 {
            return Err(ApertureBindingError::ZeroLength);
        }
        if !aperture_bytes.is_power_of_two() {
            return Err(ApertureBindingError::NotPowerOfTwo);
        }
        if aperture_bytes > self.config.bootstrap_containment_bytes {
            return Err(ApertureBindingError::ExceedsBootstrapContainment);
        }
        if required_transaction_end(self.config).is_none_or(|required| required > aperture_bytes) {
            return Err(ApertureBindingError::DoesNotContainTransactionRegisters);
        }
        self.measured_aperture_bytes = Some(aperture_bytes);
        self.phase = TakeoverPhase::RequestReset;
        Ok(())
    }

    pub fn step<R, D>(
        &mut self,
        registers: &mut R,
        deadline: &mut D,
    ) -> Result<TakeoverObservation, TakeoverFault<R::Error, D::Error>>
    where
        R: RegisterIo,
        D: RelativeDeadline,
    {
        if self.phase == TakeoverPhase::AwaitMeasuredAperture {
            return Err(TakeoverFault::MeasuredApertureRequired);
        }
        if matches!(
            self.phase,
            TakeoverPhase::ReadyHalted | TakeoverPhase::Faulted
        ) {
            return Err(TakeoverFault::TerminalState(self.phase));
        }
        let result = self.step_inner(registers, deadline);
        if result.is_err() {
            self.phase = TakeoverPhase::Faulted;
        }
        result
    }

    fn step_inner<R, D>(
        &mut self,
        registers: &mut R,
        deadline: &mut D,
    ) -> Result<TakeoverObservation, TakeoverFault<R::Error, D::Error>>
    where
        R: RegisterIo,
        D: RelativeDeadline,
    {
        match self.phase {
            TakeoverPhase::ClaimLegacyOwnership => {
                let offset = self.legacy_offset(LEGACY_OS_SEMAPHORE)?;
                self.write8(registers, offset, 1)?;
                Ok(self.transition(
                    TakeoverPhase::ClaimLegacyOwnership,
                    TakeoverPhase::ArmLegacyHandoff,
                ))
            }
            TakeoverPhase::ArmLegacyHandoff => self.arm(
                deadline,
                FIRMWARE_HANDOFF_DEADLINE_NS,
                TakeoverPhase::AwaitLegacyHandoff,
            ),
            TakeoverPhase::AwaitLegacyHandoff => {
                let bios = self.read8(registers, self.legacy_offset(LEGACY_BIOS_SEMAPHORE)?)?;
                let os = self.read8(registers, self.legacy_offset(LEGACY_OS_SEMAPHORE)?)?;
                let next = if bios == 0 && os != 0 {
                    self.cancel(deadline)?;
                    TakeoverPhase::MaskLegacySmi
                } else {
                    self.poll(deadline, WaitClass::FirmwareHandoff)?;
                    TakeoverPhase::AwaitLegacyHandoff
                };
                self.phase = next;
                Ok(TakeoverObservation::LegacyOwnership {
                    bios_owned: bios != 0,
                    os_owned: os != 0,
                    next,
                })
            }
            TakeoverPhase::MaskLegacySmi => {
                let offset = self.legacy_offset(LEGACY_CONTROL_STATUS)?;
                let observed = self.read32(registers, offset)?;
                let written = observed & !LEGACY_SMI_ENABLES & !LEGACY_RW1C_STATUS;
                self.write32(registers, offset, written)?;
                self.phase = TakeoverPhase::VerifyLegacySmiMasked;
                Ok(TakeoverObservation::LegacySmiMasked {
                    observed,
                    written,
                    next: self.phase,
                })
            }
            TakeoverPhase::VerifyLegacySmiMasked => {
                let readback =
                    self.read32(registers, self.legacy_offset(LEGACY_CONTROL_STATUS)?)?;
                if readback & LEGACY_SMI_ENABLES != 0 {
                    return Err(TakeoverFault::LegacySmiMaskRejected { readback });
                }
                self.phase = TakeoverPhase::ArmInitialReady;
                Ok(TakeoverObservation::LegacySmiVerified {
                    readback,
                    next: self.phase,
                })
            }
            TakeoverPhase::ArmInitialReady => self.arm(
                deadline,
                INITIAL_READY_DEADLINE_NS,
                TakeoverPhase::AwaitInitialReady,
            ),
            TakeoverPhase::AwaitInitialReady => {
                let status = self.read_status(registers)?;
                self.reject_host_controller_error(status)?;
                let next = if status & STATUS_CONTROLLER_NOT_READY == 0 {
                    self.cancel(deadline)?;
                    TakeoverPhase::ArmUsb2ResetDrain
                } else {
                    self.poll(deadline, WaitClass::InitialControllerReady)?;
                    TakeoverPhase::AwaitInitialReady
                };
                self.phase = next;
                Ok(TakeoverObservation::ControllerStatus {
                    completed: TakeoverPhase::AwaitInitialReady,
                    status,
                    next,
                })
            }
            TakeoverPhase::ArmUsb2ResetDrain => self.arm(
                deadline,
                USB2_RESET_DRAIN_DEADLINE_NS,
                TakeoverPhase::DrainUsb2Resets,
            ),
            TakeoverPhase::DrainUsb2Resets => self.drain_one_usb2_port(registers, deadline),
            TakeoverPhase::RequestHalt => {
                let offset = self.operation_offset(USB_COMMAND);
                let observed = self.read32(registers, offset)?;
                if observed & COMMAND_HOST_CONTROLLER_RESET != 0 {
                    return Err(TakeoverFault::IllegalControllerState {
                        phase: self.phase,
                        command: observed,
                        status: 0,
                    });
                }
                let written = observed & !COMMAND_RUN_STOP;
                self.write32(registers, offset, written)?;
                self.phase = TakeoverPhase::ArmHalt;
                Ok(TakeoverObservation::ControllerCommand {
                    completed: TakeoverPhase::RequestHalt,
                    observed,
                    written,
                    next: self.phase,
                })
            }
            TakeoverPhase::ArmHalt => {
                self.arm(deadline, HALT_DEADLINE_NS, TakeoverPhase::AwaitHalt)
            }
            TakeoverPhase::AwaitHalt => {
                let status = self.read_status(registers)?;
                self.reject_host_controller_error(status)?;
                let next = if status & STATUS_HOST_CONTROLLER_HALTED != 0 {
                    self.cancel(deadline)?;
                    TakeoverPhase::AwaitMeasuredAperture
                } else {
                    self.poll(deadline, WaitClass::ControllerHalt)?;
                    TakeoverPhase::AwaitHalt
                };
                self.phase = next;
                Ok(TakeoverObservation::ControllerStatus {
                    completed: TakeoverPhase::AwaitHalt,
                    status,
                    next,
                })
            }
            TakeoverPhase::AwaitMeasuredAperture => Err(TakeoverFault::MeasuredApertureRequired),
            TakeoverPhase::RequestReset => {
                let offset = self.operation_offset(USB_COMMAND);
                let status = self.read_status(registers)?;
                self.reject_host_controller_error(status)?;
                let observed = self.read32(registers, offset)?;
                if observed & (COMMAND_RUN_STOP | COMMAND_HOST_CONTROLLER_RESET) != 0
                    || status & STATUS_HOST_CONTROLLER_HALTED == 0
                    || status & STATUS_CONTROLLER_NOT_READY != 0
                {
                    return Err(TakeoverFault::IllegalControllerState {
                        phase: self.phase,
                        command: observed,
                        status,
                    });
                }
                let written = observed | COMMAND_HOST_CONTROLLER_RESET;
                self.write32(registers, offset, written)?;
                self.phase = TakeoverPhase::ArmReset;
                Ok(TakeoverObservation::ControllerCommand {
                    completed: TakeoverPhase::RequestReset,
                    observed,
                    written,
                    next: self.phase,
                })
            }
            TakeoverPhase::ArmReset => {
                self.arm(deadline, RESET_DEADLINE_NS, TakeoverPhase::AwaitReset)
            }
            TakeoverPhase::AwaitReset => {
                let offset = self.operation_offset(USB_COMMAND);
                let command = self.read32(registers, offset)?;
                let next = if command & COMMAND_HOST_CONTROLLER_RESET == 0 {
                    self.cancel(deadline)?;
                    TakeoverPhase::ArmPostResetReady
                } else {
                    self.poll(deadline, WaitClass::ControllerReset)?;
                    TakeoverPhase::AwaitReset
                };
                self.phase = next;
                Ok(TakeoverObservation::ControllerCommand {
                    completed: TakeoverPhase::AwaitReset,
                    observed: command,
                    written: command,
                    next,
                })
            }
            TakeoverPhase::ArmPostResetReady => self.arm(
                deadline,
                POST_RESET_READY_DEADLINE_NS,
                TakeoverPhase::AwaitPostResetReady,
            ),
            TakeoverPhase::AwaitPostResetReady => {
                let status = self.read_status(registers)?;
                self.reject_host_controller_error(status)?;
                let next = if status & STATUS_CONTROLLER_NOT_READY == 0 {
                    self.cancel(deadline)?;
                    TakeoverPhase::VerifyReadyHalted
                } else {
                    self.poll(deadline, WaitClass::PostResetControllerReady)?;
                    TakeoverPhase::AwaitPostResetReady
                };
                self.phase = next;
                Ok(TakeoverObservation::ControllerStatus {
                    completed: TakeoverPhase::AwaitPostResetReady,
                    status,
                    next,
                })
            }
            TakeoverPhase::VerifyReadyHalted => {
                let command = self.read32(registers, self.operation_offset(USB_COMMAND))?;
                let status = self.read_status(registers)?;
                let command_clear =
                    command & (COMMAND_RUN_STOP | COMMAND_HOST_CONTROLLER_RESET) == 0;
                let status_ready = status & STATUS_CONTROLLER_NOT_READY == 0;
                let status_halted = status & STATUS_HOST_CONTROLLER_HALTED != 0;
                let status_error_free = status & STATUS_HOST_CONTROLLER_ERROR == 0;
                if !command_clear || !status_ready || !status_halted || !status_error_free {
                    return Err(TakeoverFault::ReadyHaltedInvariant { command, status });
                }
                let ready = ReadyHalted {
                    command,
                    status,
                    measured_aperture_bytes: self
                        .measured_aperture_bytes
                        .ok_or(TakeoverFault::MeasuredApertureRequired)?,
                    legacy_handoff_performed: self.config.legacy_support_offset.is_some(),
                    ports_observed: self.ports_observed,
                };
                self.phase = TakeoverPhase::ReadyHalted;
                Ok(TakeoverObservation::Ready(ready))
            }
            TakeoverPhase::ReadyHalted | TakeoverPhase::Faulted => {
                Err(TakeoverFault::TerminalState(self.phase))
            }
        }
    }

    fn arm<R, D>(
        &mut self,
        deadline: &mut D,
        duration_ns: u64,
        next: TakeoverPhase,
    ) -> Result<TakeoverObservation, TakeoverFault<R, D::Error>>
    where
        D: RelativeDeadline,
    {
        let completed = self.phase;
        deadline
            .arm(duration_ns)
            .map_err(|source| TakeoverFault::Deadline {
                phase: completed,
                operation: DeadlineOperation::Arm,
                source,
            })?;
        self.phase = next;
        Ok(TakeoverObservation::Transition { completed, next })
    }

    fn poll<R, D>(
        &self,
        deadline: &mut D,
        wait: WaitClass,
    ) -> Result<(), TakeoverFault<R, D::Error>>
    where
        D: RelativeDeadline,
    {
        match deadline.poll().map_err(|source| TakeoverFault::Deadline {
            phase: self.phase,
            operation: DeadlineOperation::Poll,
            source,
        })? {
            RelativeDeadlineState::Pending => Ok(()),
            RelativeDeadlineState::Expired => Err(TakeoverFault::Timeout(wait)),
        }
    }

    fn cancel<R, D>(&self, deadline: &mut D) -> Result<(), TakeoverFault<R, D::Error>>
    where
        D: RelativeDeadline,
    {
        deadline.cancel().map_err(|source| TakeoverFault::Deadline {
            phase: self.phase,
            operation: DeadlineOperation::Cancel,
            source,
        })
    }

    fn drain_one_usb2_port<R, D>(
        &mut self,
        registers: &mut R,
        deadline: &mut D,
    ) -> Result<TakeoverObservation, TakeoverFault<R::Error, D::Error>>
    where
        R: RegisterIo,
        D: RelativeDeadline,
    {
        let protocol_evidenced = !self.config.usb2_protocols.is_empty();
        let (port_id, cycle_complete) = if protocol_evidenced {
            let range = self.config.usb2_protocols[self.usb2_range_index];
            let port_id = range.first_port + self.usb2_port_in_range;
            if self.usb2_port_in_range + 1 == range.port_count {
                self.usb2_port_in_range = 0;
                self.usb2_range_index += 1;
            } else {
                self.usb2_port_in_range += 1;
            }
            (
                port_id,
                self.usb2_range_index == self.config.usb2_protocols.len(),
            )
        } else {
            let port_id = self.usb2_port_in_range + 1;
            if port_id == self.config.maximum_ports {
                self.usb2_port_in_range = 0;
                (port_id, true)
            } else {
                self.usb2_port_in_range += 1;
                (port_id, false)
            }
        };
        let port_status = self.read32(registers, self.port_offset(port_id))?;
        self.ports_observed = self.ports_observed.saturating_add(1);
        self.usb2_cycle_saw_reset |= port_status & PORT_RESET != 0;

        if cycle_complete {
            if self.usb2_cycle_saw_reset {
                self.poll(deadline, WaitClass::Usb2PortResetDrain)?;
                self.usb2_range_index = 0;
                self.usb2_cycle_saw_reset = false;
            } else {
                self.cancel(deadline)?;
                self.phase = TakeoverPhase::RequestHalt;
            }
        }

        Ok(TakeoverObservation::PortResetDrain {
            port_id,
            port_status,
            usb2_protocol_evidenced: protocol_evidenced,
            next: self.phase,
        })
    }

    fn reject_host_controller_error<RegisterError, DeadlineError>(
        &self,
        status: u32,
    ) -> Result<(), TakeoverFault<RegisterError, DeadlineError>> {
        if status & STATUS_HOST_CONTROLLER_ERROR != 0 {
            Err(TakeoverFault::HostControllerError {
                phase: self.phase,
                status,
            })
        } else {
            Ok(())
        }
    }

    fn transition(&mut self, completed: TakeoverPhase, next: TakeoverPhase) -> TakeoverObservation {
        self.phase = next;
        TakeoverObservation::Transition { completed, next }
    }

    fn read8<R, D>(&self, registers: &mut R, offset: u32) -> Result<u8, TakeoverFault<R::Error, D>>
    where
        R: RegisterIo,
    {
        registers
            .read8(offset)
            .map_err(|source| TakeoverFault::Register {
                phase: self.phase,
                operation: RegisterOperation::Read8,
                offset,
                source,
            })
    }

    fn write8<R, D>(
        &self,
        registers: &mut R,
        offset: u32,
        value: u8,
    ) -> Result<(), TakeoverFault<R::Error, D>>
    where
        R: RegisterIo,
    {
        registers
            .write8(offset, value)
            .map_err(|source| TakeoverFault::Register {
                phase: self.phase,
                operation: RegisterOperation::Write8,
                offset,
                source,
            })
    }

    fn read32<R, D>(
        &self,
        registers: &mut R,
        offset: u32,
    ) -> Result<u32, TakeoverFault<R::Error, D>>
    where
        R: RegisterIo,
    {
        registers
            .read32(offset)
            .map_err(|source| TakeoverFault::Register {
                phase: self.phase,
                operation: RegisterOperation::Read32,
                offset,
                source,
            })
    }

    fn write32<R, D>(
        &self,
        registers: &mut R,
        offset: u32,
        value: u32,
    ) -> Result<(), TakeoverFault<R::Error, D>>
    where
        R: RegisterIo,
    {
        registers
            .write32(offset, value)
            .map_err(|source| TakeoverFault::Register {
                phase: self.phase,
                operation: RegisterOperation::Write32,
                offset,
                source,
            })
    }

    fn read_status<R, D>(&self, registers: &mut R) -> Result<u32, TakeoverFault<R::Error, D>>
    where
        R: RegisterIo,
    {
        self.read32(registers, self.operation_offset(USB_STATUS))
    }

    fn legacy_offset<R, D>(&self, relative: u32) -> Result<u32, TakeoverFault<R, D>> {
        self.config
            .legacy_support_offset
            .and_then(|offset| offset.checked_add(relative))
            .ok_or(TakeoverFault::IllegalTransition(self.phase))
    }

    fn operation_offset(&self, relative: u32) -> u32 {
        self.config.operational_offset + relative
    }

    fn port_offset(&self, port_id: u8) -> u32 {
        self.config.operational_offset
            + PORT_REGISTER_BASE
            + u32::from(port_id - 1) * PORT_REGISTER_STRIDE
    }
}

fn validate_config(config: TakeoverConfig<'_>) -> Result<(), TakeoverConfigError> {
    if config.operational_offset < 0x20 || config.operational_offset & 3 != 0 {
        return Err(TakeoverConfigError::MisalignedOperationalOffset);
    }
    if config.usb2_protocols.len() > MAXIMUM_USB2_PROTOCOL_RANGES {
        return Err(TakeoverConfigError::TooManyUsb2ProtocolRanges);
    }
    if config.maximum_ports == 0 {
        return Err(TakeoverConfigError::InvalidMaximumPorts);
    }
    if let Some(legacy) = config.legacy_support_offset {
        let end = legacy
            .checked_add(8)
            .ok_or(TakeoverConfigError::OffsetOverflow)?;
        if legacy < 0x20 || legacy & 3 != 0 || u64::from(end) > config.bootstrap_containment_bytes {
            return Err(TakeoverConfigError::InvalidLegacyOffset);
        }
    }

    let mut occupied_ports = [0_u64; 4];
    for range in config.usb2_protocols {
        if range.first_port == 0 || range.port_count == 0 {
            return Err(TakeoverConfigError::InvalidUsb2ProtocolRange);
        }
        let last = range
            .first_port
            .checked_add(range.port_count - 1)
            .ok_or(TakeoverConfigError::InvalidUsb2ProtocolRange)?;
        if last > config.maximum_ports {
            return Err(TakeoverConfigError::InvalidUsb2ProtocolRange);
        }
        for port in range.first_port..=last {
            let zero_based = usize::from(port - 1);
            let word = zero_based / 64;
            let bit = 1_u64 << (zero_based % 64);
            if occupied_ports[word] & bit != 0 {
                return Err(TakeoverConfigError::OverlappingUsb2ProtocolRange);
            }
            occupied_ports[word] |= bit;
        }
    }
    let required = required_transaction_end(config).ok_or(TakeoverConfigError::OffsetOverflow)?;
    if required > config.bootstrap_containment_bytes {
        return Err(TakeoverConfigError::BootstrapContainmentTooSmall);
    }
    Ok(())
}

fn required_transaction_end(config: TakeoverConfig<'_>) -> Option<u64> {
    let operation_end = config.operational_offset.checked_add(8)?;
    let ports_end = config
        .operational_offset
        .checked_add(PORT_REGISTER_BASE)?
        .checked_add(u32::from(config.maximum_ports - 1) * PORT_REGISTER_STRIDE)?
        .checked_add(4)?;
    let legacy_end = match config.legacy_support_offset {
        Some(offset) => Some(offset.checked_add(8)?),
        None => None,
    };
    Some(u64::from(
        operation_end.max(ports_end).max(legacy_end.unwrap_or(0)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPERATIONAL: u32 = 0x40;
    const LEGACY: u32 = 0x100;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeRegisterError {
        Injected,
        OutOfBounds,
    }

    struct FakeRegisters {
        bytes: [u8; 4096],
        fail_offset: Option<u32>,
        release_firmware: bool,
        halt_immediately: bool,
        reset_immediately: bool,
        ignore_legacy_mask: bool,
        command_writes: usize,
        reset_requests: usize,
    }

    impl FakeRegisters {
        fn qemu() -> Self {
            let mut registers = Self {
                bytes: [0; 4096],
                fail_offset: None,
                release_firmware: true,
                halt_immediately: true,
                reset_immediately: true,
                ignore_legacy_mask: false,
                command_writes: 0,
                reset_requests: 0,
            };
            registers.set32(OPERATIONAL + USB_STATUS, STATUS_HOST_CONTROLLER_HALTED);
            registers
        }

        fn set32(&mut self, offset: u32, value: u32) {
            let start = offset as usize;
            self.bytes[start..start + 4].copy_from_slice(&value.to_le_bytes());
        }

        fn get32(&self, offset: u32) -> u32 {
            let start = offset as usize;
            u32::from_le_bytes(self.bytes[start..start + 4].try_into().unwrap())
        }

        fn checked(&self, offset: u32, bytes: usize) -> Result<usize, FakeRegisterError> {
            if self.fail_offset == Some(offset) {
                return Err(FakeRegisterError::Injected);
            }
            let start = offset as usize;
            if start
                .checked_add(bytes)
                .is_none_or(|end| end > self.bytes.len())
            {
                return Err(FakeRegisterError::OutOfBounds);
            }
            Ok(start)
        }
    }

    impl RegisterIo for FakeRegisters {
        type Error = FakeRegisterError;

        fn read8(&mut self, offset: u32) -> Result<u8, Self::Error> {
            let start = self.checked(offset, 1)?;
            Ok(self.bytes[start])
        }

        fn write8(&mut self, offset: u32, value: u8) -> Result<(), Self::Error> {
            let start = self.checked(offset, 1)?;
            self.bytes[start] = value;
            if offset == LEGACY + LEGACY_OS_SEMAPHORE && self.release_firmware {
                self.bytes[(LEGACY + LEGACY_BIOS_SEMAPHORE) as usize] = 0;
            }
            Ok(())
        }

        fn read32(&mut self, offset: u32) -> Result<u32, Self::Error> {
            let start = self.checked(offset, 4)?;
            Ok(u32::from_le_bytes(
                self.bytes[start..start + 4].try_into().unwrap(),
            ))
        }

        fn write32(&mut self, offset: u32, mut value: u32) -> Result<(), Self::Error> {
            self.checked(offset, 4)?;
            if offset == LEGACY + LEGACY_CONTROL_STATUS && self.ignore_legacy_mask {
                return Ok(());
            }
            if offset == OPERATIONAL + USB_COMMAND {
                self.command_writes += 1;
                if value & COMMAND_RUN_STOP == 0 && self.halt_immediately {
                    let status =
                        self.get32(OPERATIONAL + USB_STATUS) | STATUS_HOST_CONTROLLER_HALTED;
                    self.set32(OPERATIONAL + USB_STATUS, status);
                }
                if value & COMMAND_HOST_CONTROLLER_RESET != 0 && self.reset_immediately {
                    self.reset_requests += 1;
                    value &= !COMMAND_HOST_CONTROLLER_RESET;
                    let status =
                        self.get32(OPERATIONAL + USB_STATUS) & !STATUS_CONTROLLER_NOT_READY;
                    self.set32(
                        OPERATIONAL + USB_STATUS,
                        status | STATUS_HOST_CONTROLLER_HALTED,
                    );
                } else if value & COMMAND_HOST_CONTROLLER_RESET != 0 {
                    self.reset_requests += 1;
                }
            }
            self.set32(offset, value);
            Ok(())
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeDeadlineError {
        Injected,
        NotArmed,
    }

    struct FakeDeadline {
        arms: [u64; 8],
        arm_count: usize,
        polls_until_expired: usize,
        polls: usize,
        armed: bool,
        fail_arm: bool,
    }

    impl FakeDeadline {
        fn new(polls_until_expired: usize) -> Self {
            Self {
                arms: [0; 8],
                arm_count: 0,
                polls_until_expired,
                polls: 0,
                armed: false,
                fail_arm: false,
            }
        }
    }

    impl RelativeDeadline for FakeDeadline {
        type Error = FakeDeadlineError;

        fn arm(&mut self, duration_ns: u64) -> Result<(), Self::Error> {
            if self.fail_arm {
                return Err(FakeDeadlineError::Injected);
            }
            self.arms[self.arm_count] = duration_ns;
            self.arm_count += 1;
            self.polls = 0;
            self.armed = true;
            Ok(())
        }

        fn poll(&mut self) -> Result<RelativeDeadlineState, Self::Error> {
            if !self.armed {
                return Err(FakeDeadlineError::NotArmed);
            }
            if self.polls >= self.polls_until_expired {
                self.armed = false;
                Ok(RelativeDeadlineState::Expired)
            } else {
                self.polls += 1;
                Ok(RelativeDeadlineState::Pending)
            }
        }

        fn cancel(&mut self) -> Result<(), Self::Error> {
            if !self.armed {
                return Err(FakeDeadlineError::NotArmed);
            }
            self.armed = false;
            Ok(())
        }
    }

    fn config<'a>(protocols: &'a [Usb2ProtocolPorts]) -> TakeoverConfig<'a> {
        TakeoverConfig {
            bootstrap_containment_bytes: 4096,
            operational_offset: OPERATIONAL,
            legacy_support_offset: None,
            maximum_ports: 8,
            usb2_protocols: protocols,
        }
    }

    fn drive_ready(
        machine: &mut TakeoverMachine<'_>,
        registers: &mut FakeRegisters,
        deadline: &mut FakeDeadline,
    ) -> ReadyHalted {
        for _ in 0..64 {
            if machine.phase() == TakeoverPhase::AwaitMeasuredAperture {
                machine.bind_measured_aperture(4096).unwrap();
            }
            match machine.step(registers, deadline).unwrap() {
                TakeoverObservation::Ready(ready) => return ready,
                _ => {}
            }
        }
        panic!("takeover did not reach ReadyHalted");
    }

    fn drive_to_phase(
        machine: &mut TakeoverMachine<'_>,
        registers: &mut FakeRegisters,
        deadline: &mut FakeDeadline,
        phase: TakeoverPhase,
    ) {
        for _ in 0..64 {
            if machine.phase() == phase {
                return;
            }
            if machine.phase() == TakeoverPhase::AwaitMeasuredAperture {
                machine.bind_measured_aperture(4096).unwrap();
                continue;
            }
            machine.step(registers, deadline).unwrap();
        }
        panic!("takeover did not reach requested phase");
    }

    #[test]
    fn qemu_immediate_path_reaches_ready_halted_with_exact_deadlines() {
        let mut machine = TakeoverMachine::new(config(&[])).unwrap();
        let mut registers = FakeRegisters::qemu();
        registers.set32(OPERATIONAL + USB_COMMAND, COMMAND_RUN_STOP);
        registers.set32(OPERATIONAL + USB_STATUS, 0);
        let mut deadline = FakeDeadline::new(8);

        let ready = drive_ready(&mut machine, &mut registers, &mut deadline);

        assert!(machine.is_ready_halted());
        assert_eq!(
            ready.command() & (COMMAND_RUN_STOP | COMMAND_HOST_CONTROLLER_RESET),
            0
        );
        assert_ne!(ready.status() & STATUS_HOST_CONTROLLER_HALTED, 0);
        assert_eq!(ready.status() & STATUS_CONTROLLER_NOT_READY, 0);
        assert_eq!(ready.ports_observed(), 8);
        assert_eq!(ready.measured_aperture_bytes(), 4096);
        assert_eq!(
            &deadline.arms[..deadline.arm_count],
            &[
                INITIAL_READY_DEADLINE_NS,
                USB2_RESET_DRAIN_DEADLINE_NS,
                HALT_DEADLINE_NS,
                RESET_DEADLINE_NS,
                POST_RESET_READY_DEADLINE_NS,
            ]
        );
    }

    #[test]
    fn legacy_handoff_uses_byte_semaphores_and_masks_smi_without_acknowledging_status() {
        let mut configured = config(&[]);
        configured.legacy_support_offset = Some(LEGACY);
        let mut machine = TakeoverMachine::new(configured).unwrap();
        let mut registers = FakeRegisters::qemu();
        registers.bytes[(LEGACY + LEGACY_BIOS_SEMAPHORE) as usize] = 1;
        let control = 0xe5aa_ffff;
        registers.set32(LEGACY + LEGACY_CONTROL_STATUS, control);
        let mut deadline = FakeDeadline::new(8);

        let ready = drive_ready(&mut machine, &mut registers, &mut deadline);

        assert!(ready.legacy_handoff_performed());
        assert_eq!(registers.bytes[(LEGACY + LEGACY_OS_SEMAPHORE) as usize], 1);
        assert_eq!(
            registers.bytes[(LEGACY + LEGACY_BIOS_SEMAPHORE) as usize],
            0
        );
        assert_eq!(
            registers.get32(LEGACY + LEGACY_CONTROL_STATUS),
            control & !LEGACY_SMI_ENABLES & !LEGACY_RW1C_STATUS
        );
        assert_eq!(deadline.arms[0], FIRMWARE_HANDOFF_DEADLINE_NS);
    }

    #[test]
    fn ignored_legacy_smi_mask_write_faults_before_any_usb_command_write() {
        let mut configured = config(&[]);
        configured.legacy_support_offset = Some(LEGACY);
        let mut machine = TakeoverMachine::new(configured).unwrap();
        let mut registers = FakeRegisters::qemu();
        registers.bytes[(LEGACY + LEGACY_BIOS_SEMAPHORE) as usize] = 1;
        registers.set32(LEGACY + LEGACY_CONTROL_STATUS, LEGACY_SMI_ENABLES);
        registers.ignore_legacy_mask = true;
        let mut deadline = FakeDeadline::new(8);

        let fault = loop {
            match machine.step(&mut registers, &mut deadline) {
                Ok(_) => {}
                Err(fault) => break fault,
            }
        };

        assert_eq!(
            fault,
            TakeoverFault::LegacySmiMaskRejected {
                readback: LEGACY_SMI_ENABLES
            }
        );
        assert_eq!(machine.phase(), TakeoverPhase::Faulted);
        assert_eq!(registers.command_writes, 0);
        assert_eq!(deadline.arm_count, 1);
    }

    #[test]
    fn protocol_evidence_drains_each_usb2_port_and_exposes_observations() {
        let protocols = [Usb2ProtocolPorts {
            first_port: 2,
            port_count: 2,
        }];
        let mut machine = TakeoverMachine::new(config(&protocols)).unwrap();
        let mut registers = FakeRegisters::qemu();
        let mut deadline = FakeDeadline::new(8);
        let mut ports = [0_u8; 2];
        let mut count = 0;

        for _ in 0..64 {
            if machine.phase() == TakeoverPhase::AwaitMeasuredAperture {
                machine.bind_measured_aperture(4096).unwrap();
            }
            let observation = machine.step(&mut registers, &mut deadline).unwrap();
            match observation {
                TakeoverObservation::PortResetDrain { port_id, .. } => {
                    ports[count] = port_id;
                    count += 1;
                }
                TakeoverObservation::Ready(ready) => {
                    assert_eq!(ready.ports_observed(), 2);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(ports, [2, 3]);
        assert!(deadline.arms[..deadline.arm_count].contains(&USB2_RESET_DRAIN_DEADLINE_NS));
    }

    #[test]
    fn missing_protocol_bodies_conservatively_drains_every_advertised_port() {
        let mut configured = config(&[]);
        configured.maximum_ports = 3;
        let mut machine = TakeoverMachine::new(configured).unwrap();
        let mut registers = FakeRegisters::qemu();
        let mut deadline = FakeDeadline::new(8);
        let mut observed = [0_u8; 3];
        let mut count = 0;

        while machine.phase() != TakeoverPhase::AwaitMeasuredAperture {
            if let TakeoverObservation::PortResetDrain {
                port_id,
                usb2_protocol_evidenced,
                ..
            } = machine.step(&mut registers, &mut deadline).unwrap()
            {
                assert!(!usb2_protocol_evidenced);
                observed[count] = port_id;
                count += 1;
            }
        }

        assert_eq!(observed, [1, 2, 3]);
        assert_eq!(registers.reset_requests, 0);
    }

    #[test]
    fn stuck_firmware_handoff_faults_at_fixed_deadline() {
        let mut configured = config(&[]);
        configured.legacy_support_offset = Some(LEGACY);
        let mut machine = TakeoverMachine::new(configured).unwrap();
        let mut registers = FakeRegisters::qemu();
        registers.release_firmware = false;
        registers.bytes[(LEGACY + LEGACY_BIOS_SEMAPHORE) as usize] = 1;
        let mut deadline = FakeDeadline::new(0);
        machine.step(&mut registers, &mut deadline).unwrap();
        machine.step(&mut registers, &mut deadline).unwrap();

        let fault = machine.step(&mut registers, &mut deadline).unwrap_err();
        assert_eq!(fault, TakeoverFault::Timeout(WaitClass::FirmwareHandoff));
        assert_eq!(machine.phase(), TakeoverPhase::Faulted);
    }

    #[test]
    fn stuck_initial_cnr_faults_at_fixed_deadline() {
        let mut machine = TakeoverMachine::new(config(&[])).unwrap();
        let mut registers = FakeRegisters::qemu();
        registers.set32(OPERATIONAL + USB_STATUS, STATUS_CONTROLLER_NOT_READY);
        let mut deadline = FakeDeadline::new(0);
        machine.step(&mut registers, &mut deadline).unwrap();

        assert_eq!(
            machine.step(&mut registers, &mut deadline),
            Err(TakeoverFault::Timeout(WaitClass::InitialControllerReady))
        );
    }

    #[test]
    fn stuck_usb2_reset_faults_at_fixed_deadline() {
        let mut configured = config(&[]);
        configured.maximum_ports = 1;
        let mut machine = TakeoverMachine::new(configured).unwrap();
        let mut registers = FakeRegisters::qemu();
        registers.set32(OPERATIONAL + PORT_REGISTER_BASE, PORT_RESET);
        let mut deadline = FakeDeadline::new(0);
        drive_to_phase(
            &mut machine,
            &mut registers,
            &mut deadline,
            TakeoverPhase::DrainUsb2Resets,
        );

        assert_eq!(
            machine.step(&mut registers, &mut deadline),
            Err(TakeoverFault::Timeout(WaitClass::Usb2PortResetDrain))
        );
    }

    #[test]
    fn stuck_halt_faults_after_sixteen_milliseconds() {
        let mut machine = TakeoverMachine::new(config(&[])).unwrap();
        let mut registers = FakeRegisters::qemu();
        registers.halt_immediately = false;
        registers.set32(OPERATIONAL + USB_STATUS, 0);
        let mut deadline = FakeDeadline::new(0);
        drive_to_phase(
            &mut machine,
            &mut registers,
            &mut deadline,
            TakeoverPhase::AwaitHalt,
        );

        assert_eq!(deadline.arms[2], HALT_DEADLINE_NS);
        assert_eq!(
            machine.step(&mut registers, &mut deadline),
            Err(TakeoverFault::Timeout(WaitClass::ControllerHalt))
        );
    }

    #[test]
    fn reset_authority_requires_an_exact_sufficient_aperture_binding() {
        let mut machine = TakeoverMachine::new(config(&[])).unwrap();
        let mut registers = FakeRegisters::qemu();
        let mut deadline = FakeDeadline::new(8);
        drive_to_phase(
            &mut machine,
            &mut registers,
            &mut deadline,
            TakeoverPhase::AwaitMeasuredAperture,
        );

        assert_eq!(registers.reset_requests, 0);
        assert_eq!(
            machine.step(&mut registers, &mut deadline),
            Err(TakeoverFault::MeasuredApertureRequired)
        );
        assert_eq!(machine.phase(), TakeoverPhase::AwaitMeasuredAperture);
        assert_eq!(
            machine.bind_measured_aperture(1024),
            Err(ApertureBindingError::DoesNotContainTransactionRegisters)
        );
        assert_eq!(registers.reset_requests, 0);
        assert_eq!(
            registers.get32(OPERATIONAL + USB_COMMAND) & COMMAND_HOST_CONTROLLER_RESET,
            0
        );

        machine.bind_measured_aperture(4096).unwrap();
        assert_eq!(machine.phase(), TakeoverPhase::RequestReset);
        machine.step(&mut registers, &mut deadline).unwrap();
        assert_eq!(registers.reset_requests, 1);
    }

    #[test]
    fn reset_revalidates_halt_and_final_ready_rejects_controller_error() {
        let mut halt_machine = TakeoverMachine::new(config(&[])).unwrap();
        let mut halt_registers = FakeRegisters::qemu();
        let mut halt_deadline = FakeDeadline::new(8);
        drive_to_phase(
            &mut halt_machine,
            &mut halt_registers,
            &mut halt_deadline,
            TakeoverPhase::AwaitMeasuredAperture,
        );
        halt_machine.bind_measured_aperture(4096).unwrap();
        halt_registers.set32(OPERATIONAL + USB_STATUS, 0);
        let fault = halt_machine
            .step(&mut halt_registers, &mut halt_deadline)
            .unwrap_err();
        assert_eq!(fault.class(), TakeoverFaultClass::IllegalControllerState);
        assert_eq!(halt_registers.reset_requests, 0);

        let mut final_machine = TakeoverMachine::new(config(&[])).unwrap();
        let mut final_registers = FakeRegisters::qemu();
        let mut final_deadline = FakeDeadline::new(8);
        drive_to_phase(
            &mut final_machine,
            &mut final_registers,
            &mut final_deadline,
            TakeoverPhase::VerifyReadyHalted,
        );
        final_registers.set32(
            OPERATIONAL + USB_STATUS,
            STATUS_HOST_CONTROLLER_HALTED | STATUS_HOST_CONTROLLER_ERROR,
        );
        let fault = final_machine
            .step(&mut final_registers, &mut final_deadline)
            .unwrap_err();
        assert_eq!(fault.class(), TakeoverFaultClass::ReadyHaltedInvariant);
    }

    #[test]
    fn stuck_reset_and_post_reset_cnr_have_distinct_faults() {
        let mut reset_machine = TakeoverMachine::new(config(&[])).unwrap();
        let mut reset_registers = FakeRegisters::qemu();
        reset_registers.reset_immediately = false;
        let mut reset_deadline = FakeDeadline::new(0);
        drive_to_phase(
            &mut reset_machine,
            &mut reset_registers,
            &mut reset_deadline,
            TakeoverPhase::AwaitReset,
        );
        assert_eq!(
            reset_machine.step(&mut reset_registers, &mut reset_deadline),
            Err(TakeoverFault::Timeout(WaitClass::ControllerReset))
        );

        let mut ready_machine = TakeoverMachine::new(config(&[])).unwrap();
        let mut ready_registers = FakeRegisters::qemu();
        let mut ready_deadline = FakeDeadline::new(0);
        drive_to_phase(
            &mut ready_machine,
            &mut ready_registers,
            &mut ready_deadline,
            TakeoverPhase::AwaitPostResetReady,
        );
        ready_registers.set32(
            OPERATIONAL + USB_STATUS,
            STATUS_HOST_CONTROLLER_HALTED | STATUS_CONTROLLER_NOT_READY,
        );
        assert_eq!(
            ready_machine.step(&mut ready_registers, &mut ready_deadline),
            Err(TakeoverFault::Timeout(WaitClass::PostResetControllerReady))
        );
    }

    #[test]
    fn host_error_register_error_and_deadline_error_are_classified() {
        let mut host_machine = TakeoverMachine::new(config(&[])).unwrap();
        let mut host_registers = FakeRegisters::qemu();
        host_registers.set32(OPERATIONAL + USB_STATUS, STATUS_HOST_CONTROLLER_ERROR);
        let mut deadline = FakeDeadline::new(8);
        host_machine
            .step(&mut host_registers, &mut deadline)
            .unwrap();
        let host = host_machine
            .step(&mut host_registers, &mut deadline)
            .unwrap_err();
        assert_eq!(host.class(), TakeoverFaultClass::HostControllerError);

        let mut io_machine = TakeoverMachine::new(config(&[])).unwrap();
        let mut io_registers = FakeRegisters::qemu();
        io_registers.fail_offset = Some(OPERATIONAL + USB_STATUS);
        let mut deadline = FakeDeadline::new(8);
        io_machine.step(&mut io_registers, &mut deadline).unwrap();
        let io = io_machine
            .step(&mut io_registers, &mut deadline)
            .unwrap_err();
        assert_eq!(io.class(), TakeoverFaultClass::RegisterAccess);

        let mut clock_machine = TakeoverMachine::new(config(&[])).unwrap();
        let mut clock_registers = FakeRegisters::qemu();
        let mut failing_deadline = FakeDeadline::new(8);
        failing_deadline.fail_arm = true;
        let clock = clock_machine
            .step(&mut clock_registers, &mut failing_deadline)
            .unwrap_err();
        assert_eq!(clock.class(), TakeoverFaultClass::Deadline);
    }

    #[test]
    fn validates_bootstrap_containment_and_protocol_topology() {
        let mut too_small = config(&[]);
        too_small.bootstrap_containment_bytes = u64::from(OPERATIONAL + PORT_REGISTER_BASE);
        assert!(matches!(
            TakeoverMachine::new(too_small),
            Err(TakeoverConfigError::BootstrapContainmentTooSmall)
        ));

        let overlap = [
            Usb2ProtocolPorts {
                first_port: 1,
                port_count: 3,
            },
            Usb2ProtocolPorts {
                first_port: 3,
                port_count: 2,
            },
        ];
        assert!(matches!(
            TakeoverMachine::new(config(&overlap)),
            Err(TakeoverConfigError::OverlappingUsb2ProtocolRange)
        ));

        let outside = [Usb2ProtocolPorts {
            first_port: 8,
            port_count: 2,
        }];
        assert!(matches!(
            TakeoverMachine::new(config(&outside)),
            Err(TakeoverConfigError::InvalidUsb2ProtocolRange)
        ));
    }
}
