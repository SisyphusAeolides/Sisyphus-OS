//! Drivernet — Quantum ML Universal Driver Adaptation
//!
//! Collapses the superposition of GPU driver strategies at boot using a
//! fixed-point compatibility oracle (same dialect as Golem). Consumes the
//! existing `hw::pci::PciInventory`, never re-scans CF8/CFC itself.

#![allow(dead_code)]

pub mod compat_oracle;
pub mod dispatch;
pub mod fingerprint;
pub mod shims;

use crate::hw::pci::PciInventory;
use crate::serial::SerialPort;
use core::fmt::Write;

use self::compat_oracle::{DriverStrategy, classify_gpu};
use self::dispatch::{DispatchReport, dispatch_strategy};
use self::fingerprint::{GpuFingerprint, fingerprint_display_devices};

pub const MAXIMUM_GPUS: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedGpu {
    pub fingerprint: GpuFingerprint,
    pub strategy: DriverStrategy,
    pub report: DispatchReport,
}

#[derive(Clone, Copy)]
pub struct DrivernetSummary {
    resolutions: [ResolvedGpu; MAXIMUM_GPUS],
    length: usize,
    display_functions: usize,
}

impl DrivernetSummary {
    pub const fn empty() -> Self {
        Self {
            resolutions: [ResolvedGpu {
                fingerprint: GpuFingerprint::empty(),
                strategy: DriverStrategy::VesaFallback,
                report: DispatchReport::Deferred,
            }; MAXIMUM_GPUS],
            length: 0,
            display_functions: 0,
        }
    }

    pub fn resolutions(&self) -> &[ResolvedGpu] {
        &self.resolutions[..self.length]
    }

    pub const fn display_functions(&self) -> usize {
        self.display_functions
    }
}

/// Early-boot entry. Call once after `pci::scan_buses()`, before Kairos.
///
/// Pure decision + shim arming. Does not enable interrupts. Does not allocate.
pub fn resolve_all(inventory: &PciInventory, serial: &mut SerialPort) -> DrivernetSummary {
    let mut summary = DrivernetSummary::empty();
    let mut fingerprints = [GpuFingerprint::empty(); MAXIMUM_GPUS];
    let gpu_count = fingerprint_display_devices(inventory, &mut fingerprints);
    summary.display_functions = gpu_count;

    let _ = writeln!(
        serial,
        "Drivernet: collapsing {} display function(s)",
        gpu_count
    );

    for index in 0..gpu_count {
        let fp = fingerprints[index];
        let strategy = classify_gpu(&fp);
        let report = dispatch_strategy(&fp, strategy, serial);

        let _ = writeln!(
            serial,
            "Drivernet: {:02x}:{:02x}.{} {:04x}:{:04x} class={:02x}/{:02x} bar0={}MiB -> {:?} ({:?})",
            fp.bus,
            fp.slot,
            fp.function,
            fp.vendor_id,
            fp.device_id,
            fp.class_code,
            fp.subclass,
            fp.bar0_mb(),
            strategy,
            report
        );

        if summary.length < MAXIMUM_GPUS {
            summary.resolutions[summary.length] = ResolvedGpu {
                fingerprint: fp,
                strategy,
                report,
            };
            summary.length += 1;
        }
    }

    if gpu_count == 0 {
        let _ = writeln!(
            serial,
            "Drivernet: no class-0x03 devices; arming VESA fallback"
        );
        let fp = GpuFingerprint::empty();
        let report = dispatch_strategy(&fp, DriverStrategy::VesaFallback, serial);
        summary.resolutions[0] = ResolvedGpu {
            fingerprint: fp,
            strategy: DriverStrategy::VesaFallback,
            report,
        };
        summary.length = 1;
    }

    summary
}
