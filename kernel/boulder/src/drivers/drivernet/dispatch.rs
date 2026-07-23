use super::compat_oracle::DriverStrategy;
use super::fingerprint::GpuFingerprint;
use super::shims::{amd_kms, hermes_nvidia, intel_kms, vesa_fb};
use crate::serial::SerialPort;
use core::fmt::Write;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchReport {
    /// Shim armed and accepted the device
    Armed,
    /// Shim declined; caller should try fallback (handled inside resolve cascade)
    Declined,
    /// Strategy intentionally waits (VFIO / multi-GPU hold)
    Held,
    /// Recorded for userspace / later bind (Mesa path)
    Deferred,
    /// Last-resort framebuffer claimed
    FallbackArmed,
}

pub fn dispatch_strategy(
    fp: &GpuFingerprint,
    strategy: DriverStrategy,
    serial: &mut SerialPort,
) -> DispatchReport {
    match strategy {
        DriverStrategy::HermesNative => match hermes_nvidia::arm(fp, serial) {
            Ok(()) => DispatchReport::Armed,
            Err(reason) => {
                let _ = writeln!(serial, "Drivernet: Hermes arm failed: {reason}");
                // Cascade: prefer open KMS, then VESA
                if amd_kms::arm(fp, serial).is_ok() || intel_kms::arm(fp, serial).is_ok() {
                    DispatchReport::Armed
                } else if vesa_fb::arm(fp, serial).is_ok() {
                    DispatchReport::FallbackArmed
                } else {
                    DispatchReport::Declined
                }
            }
        },
        DriverStrategy::MesaOpen => {
            let ok = amd_kms::arm(fp, serial).is_ok() || intel_kms::arm(fp, serial).is_ok();
            if ok {
                DispatchReport::Deferred
            } else if vesa_fb::arm(fp, serial).is_ok() {
                DispatchReport::FallbackArmed
            } else {
                DispatchReport::Declined
            }
        }
        DriverStrategy::DrmKmsOnly => {
            if amd_kms::arm(fp, serial).is_ok()
                || intel_kms::arm(fp, serial).is_ok()
                || vesa_fb::arm(fp, serial).is_ok()
            {
                DispatchReport::Armed
            } else {
                DispatchReport::Declined
            }
        }
        DriverStrategy::VfioHold => {
            let _ = writeln!(
                serial,
                "Drivernet: VFIO hold on {:02x}:{:02x}.{} (hybrid/split)",
                fp.bus, fp.slot, fp.function
            );
            DispatchReport::Held
        }
        DriverStrategy::VesaFallback => {
            if vesa_fb::arm(fp, serial).is_ok() {
                DispatchReport::FallbackArmed
            } else {
                DispatchReport::Declined
            }
        }
    }
}
