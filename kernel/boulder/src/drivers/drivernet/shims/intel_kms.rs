use crate::drivers::drivernet::fingerprint::{GpuFingerprint, VENDOR_INTEL};
use crate::serial::SerialPort;
use core::fmt::Write;

pub fn arm(fp: &GpuFingerprint, serial: &mut SerialPort) -> Result<(), &'static str> {
    if fp.vendor_id != VENDOR_INTEL {
        return Err("not-intel");
    }
    let _ = writeln!(
        serial,
        "Drivernet/Intel: KMS claim {:04x}:{:04x} (i915/Xe personality stub)",
        fp.vendor_id, fp.device_id
    );
    Ok(())
}
