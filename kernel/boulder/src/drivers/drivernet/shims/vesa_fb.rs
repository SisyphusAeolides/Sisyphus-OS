//! Universal last resort — always succeeds so a capable x86-64 machine gets a console path.

use crate::drivers::drivernet::fingerprint::GpuFingerprint;
use crate::serial::SerialPort;
use core::fmt::Write;

pub fn arm(fp: &GpuFingerprint, serial: &mut SerialPort) -> Result<(), &'static str> {
    let _ = writeln!(
        serial,
        "Drivernet/VESA: linear-fb fallback armed (vendor={:04x} device={:04x})",
        fp.vendor_id, fp.device_id
    );
    Ok(())
}
