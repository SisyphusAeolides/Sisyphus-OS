//! Arms the existing Hermes GSP path for NVIDIA display functions.

use crate::drivers::drivernet::fingerprint::{GpuFingerprint, PCI_CLASS_DISPLAY, VENDOR_NVIDIA};
use crate::drivers::hermes_gsp;
use crate::serial::SerialPort;
use core::fmt::Write;

pub fn arm(fp: &GpuFingerprint, serial: &mut SerialPort) -> Result<(), &'static str> {
    if fp.vendor_id != VENDOR_NVIDIA {
        return Err("not-nvidia");
    }
    if fp.class_code != PCI_CLASS_DISPLAY {
        return Err("not-display");
    }

    let identity = fp.to_hermes_identity();
    // Validate with the same gate hermes_gsp unit tests use.
    // Full bind/stage/ignite requires a HermesPlatform backend + firmware;
    // at drivernet time we only claim + publish the identity for later ignite.
    if identity.vendor_id != hermes_gsp::NVIDIA_VENDOR_ID {
        return Err("hermes-reject-vendor");
    }
    if identity.class_code != hermes_gsp::PCI_CLASS_DISPLAY {
        return Err("hermes-reject-class");
    }

    let _ = writeln!(
        serial,
        "Drivernet/Hermes: claimed NVIDIA {:04x}:{:04x} at {:02x}:{:02x}.{} (GSP path deferred to platform bind)",
        fp.device_id, fp.vendor_id, fp.bus, fp.slot, fp.function
    );
    // Future: store identity in a static claim table consumed by DriverHost
    // once firmware + IOMMU domain are available post-subsystems_ready.
    let _ = identity;
    Ok(())
}
