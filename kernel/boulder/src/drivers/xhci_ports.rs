//! Read-only xHCI root-port census while the controller remains halted.
//!
//! This is deliberately before DMA, bus mastering, interrupter setup, and
//! child enumeration. It turns raw PortSC state into bounded retained evidence
//! without claiming that a connected device is operational.

use super::xhci::XhciResetReadyController;
use super::xhci_protocol::UsbProtocolKind;
use super::xhci_runtime::XhciRuntimeRegisters;

const USBCMD_RUN_STOP: u32 = 1 << 0;
const USBSTS_HCHALTED: u32 = 1 << 0;
const USBSTS_HOST_CONTROLLER_ERROR: u32 = 1 << 2;
const PORTSC_BASE: u32 = 0x400;
const PORTSC_STRIDE: u32 = 0x10;
const PORT_CONNECT: u32 = 1 << 0;
const PORT_ENABLED: u32 = 1 << 1;
const PORT_OVERCURRENT: u32 = 1 << 3;
const PORT_RESET: u32 = 1 << 4;
const PORT_LINK_STATE_SHIFT: u32 = 5;
const PORT_LINK_STATE_MASK: u32 = 0x0f << PORT_LINK_STATE_SHIFT;
const PORT_SPEED_SHIFT: u32 = 10;
const PORT_SPEED_MASK: u32 = 0x0f << PORT_SPEED_SHIFT;
const PORT_ROOT_DOMAIN: u64 = 0x5848_4349_504f_5254;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XhciPortProtocol {
    Usb2,
    Usb3,
    Unclassified,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XhciPortObservation {
    pub port_id: u8,
    pub protocol: XhciPortProtocol,
    pub connected: bool,
    pub enabled: bool,
    pub reset_active: bool,
    pub overcurrent: bool,
    pub link_state: u8,
    pub speed_id: u8,
    pub portsc: u32,
}

impl XhciPortObservation {
    const EMPTY: Self = Self {
        port_id: 0,
        protocol: XhciPortProtocol::Unclassified,
        connected: false,
        enabled: false,
        reset_active: false,
        overcurrent: false,
        link_state: 0,
        speed_id: 0,
        portsc: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XhciPortSurvey {
    observations: [XhciPortObservation; u8::MAX as usize],
    count: u8,
    pub connected_ports: u8,
    pub enabled_ports: u8,
    pub reset_active_ports: u8,
    pub overcurrent_ports: u8,
    pub root: u64,
}

impl XhciPortSurvey {
    pub fn observations(&self) -> &[XhciPortObservation] {
        &self.observations[..usize::from(self.count)]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XhciPortSurveyInvariant {
    InvalidSecret,
    ControllerNotHalted,
    ControllerError,
    RegisterOffsetOverflow,
    RegisterOutsideAperture,
}

#[derive(Debug, Eq, PartialEq)]
pub enum XhciPortSurveyError<RegisterError> {
    Invariant(XhciPortSurveyInvariant),
    Register(RegisterError),
}

pub fn survey_halted_ports<R: XhciRuntimeRegisters>(
    controller: &XhciResetReadyController,
    registers: &mut R,
    secret: u64,
) -> Result<XhciPortSurvey, XhciPortSurveyError<R::Error>> {
    if secret == 0 {
        return Err(XhciPortSurveyError::Invariant(
            XhciPortSurveyInvariant::InvalidSecret,
        ));
    }
    let snapshot = controller.snapshot();
    let operational_offset = u32::from(snapshot.capability_length);
    let status_offset = checked_offset(operational_offset, 4)?;
    let command = registers
        .read32(operational_offset)
        .map_err(XhciPortSurveyError::Register)?;
    let status = registers
        .read32(status_offset)
        .map_err(XhciPortSurveyError::Register)?;
    if command & USBCMD_RUN_STOP != 0 || status & USBSTS_HCHALTED == 0 {
        return Err(XhciPortSurveyError::Invariant(
            XhciPortSurveyInvariant::ControllerNotHalted,
        ));
    }
    if status & USBSTS_HOST_CONTROLLER_ERROR != 0 {
        return Err(XhciPortSurveyError::Invariant(
            XhciPortSurveyInvariant::ControllerError,
        ));
    }

    let mut survey = XhciPortSurvey {
        observations: [XhciPortObservation::EMPTY; u8::MAX as usize],
        count: snapshot.maximum_ports,
        connected_ports: 0,
        enabled_ports: 0,
        reset_active_ports: 0,
        overcurrent_ports: 0,
        root: 0,
    };
    let mut root = mix(secret ^ PORT_ROOT_DOMAIN, controller.reset_ready_root());
    for port_id in 1..=snapshot.maximum_ports {
        let portsc_offset = operational_offset
            .checked_add(PORTSC_BASE)
            .and_then(|offset| {
                offset.checked_add(u32::from(port_id - 1).checked_mul(PORTSC_STRIDE)?)
            })
            .ok_or(XhciPortSurveyError::Invariant(
                XhciPortSurveyInvariant::RegisterOffsetOverflow,
            ))?;
        let end = u64::from(portsc_offset)
            .checked_add(4)
            .ok_or(XhciPortSurveyError::Invariant(
                XhciPortSurveyInvariant::RegisterOffsetOverflow,
            ))?;
        if end > controller.aperture().length() {
            return Err(XhciPortSurveyError::Invariant(
                XhciPortSurveyInvariant::RegisterOutsideAperture,
            ));
        }
        let portsc = registers
            .read32(portsc_offset)
            .map_err(XhciPortSurveyError::Register)?;
        let observation = XhciPortObservation {
            port_id,
            protocol: protocol_for_port(controller, port_id),
            connected: portsc & PORT_CONNECT != 0,
            enabled: portsc & PORT_ENABLED != 0,
            reset_active: portsc & PORT_RESET != 0,
            overcurrent: portsc & PORT_OVERCURRENT != 0,
            link_state: ((portsc & PORT_LINK_STATE_MASK) >> PORT_LINK_STATE_SHIFT) as u8,
            speed_id: ((portsc & PORT_SPEED_MASK) >> PORT_SPEED_SHIFT) as u8,
            portsc,
        };
        survey.connected_ports += u8::from(observation.connected);
        survey.enabled_ports += u8::from(observation.enabled);
        survey.reset_active_ports += u8::from(observation.reset_active);
        survey.overcurrent_ports += u8::from(observation.overcurrent);
        survey.observations[usize::from(port_id - 1)] = observation;
        root = mix(root, u64::from(port_id));
        root = mix(root, u64::from(portsc));
    }
    survey.root = if root == 0 { PORT_ROOT_DOMAIN } else { root };
    Ok(survey)
}

fn protocol_for_port(controller: &XhciResetReadyController, port_id: u8) -> XhciPortProtocol {
    controller
        .protocols()
        .protocols()
        .iter()
        .find(|protocol| {
            let last = protocol
                .first_port
                .saturating_add(protocol.port_count.saturating_sub(1));
            port_id >= protocol.first_port && port_id <= last
        })
        .map(|protocol| match protocol.kind {
            UsbProtocolKind::Usb2 => XhciPortProtocol::Usb2,
            UsbProtocolKind::Usb3 => XhciPortProtocol::Usb3,
        })
        .unwrap_or(XhciPortProtocol::Unclassified)
}

fn checked_offset<R>(base: u32, displacement: u32) -> Result<u32, XhciPortSurveyError<R>> {
    base.checked_add(displacement)
        .ok_or(XhciPortSurveyError::Invariant(
            XhciPortSurveyInvariant::RegisterOffsetOverflow,
        ))
}

fn mix(mut state: u64, word: u64) -> u64 {
    state ^= word.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}
