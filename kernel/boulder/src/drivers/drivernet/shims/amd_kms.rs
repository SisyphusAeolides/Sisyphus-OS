use crate::drivers::drivernet::fingerprint::{GpuFingerprint, VENDOR_AMD};
use crate::serial::SerialPort;
use core::fmt::Write;

pub fn arm(fp: &GpuFingerprint, serial: &mut SerialPort) -> Result<(), &'static str> {
    if fp.vendor_id != VENDOR_AMD {
        return Err("not-amd");
    }
    let _ = writeln!(
        serial,
        "Drivernet/AMD: KMS claim {:04x}:{:04x} bar0={}MiB (personality stub)",
        fp.vendor_id,
        fp.device_id,
        fp.bar0_mb()
    );
    Ok(())
}
