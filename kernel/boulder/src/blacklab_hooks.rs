// kernel/boulder/src/blacklab_hooks.rs
//! Black-lab boot/tick hooks — one call site from main.rs
//!
//! boot_after_drivernet(pci, drivernet, serial)
//!   1. seed resource quiver from PCI + drivernet
//!   2. build Hodge nerve + 2-simplices from quiver
//!   3. init cyclotomic fair-q (64-class)
//!   4. one mutate_hottest + one heat_flow for bring-up proof
//!
//! tick(now) — optional later from timer: heat + fairq quantum + mutate

#![allow(dead_code)]

use crate::cluster_quiver::{ResourceQuiver, FP_ONE, MAX_N};
use crate::cyclotomic_ntt::CyclotomicFairQ;
use crate::drivers::drivernet::DrivernetSummary;
use crate::hodge_cech::{HodgeNerve, FP_ONE as HFP_ONE};
use crate::hw::pci::PciInventory;
use crate::resource_quiver_seed::{hodge_from_quiver, seed_from_pci_and_drivernet};
use crate::serial::SerialPort;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

static READY: AtomicBool = AtomicBool::new(false);

pub struct BlackLabState {
    pub quiver: ResourceQuiver,
    pub hodge: HodgeNerve,
    pub fairq: CyclotomicFairQ,
    pub last_mutated: Option<usize>,
    pub boot_energy: u64,
    pub faces: u16,
}

impl BlackLabState {
    pub const fn empty() -> Self {
        Self {
            quiver: ResourceQuiver::new(0),
            hodge: HodgeNerve::new(0),
            fairq: CyclotomicFairQ::empty(),
            last_mutated: None,
            boot_energy: 0,
            faces: 0,
        }
    }
}

static mut STATE: BlackLabState = BlackLabState::empty();

/// # Safety
/// Single-threaded early boot, or caller holds exclusive access.
pub unsafe fn state_mut() -> &'static mut BlackLabState {
    unsafe { &mut *core::ptr::addr_of_mut!(STATE) }
}

pub fn ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// Call once after `drivernet::resolve_all`, before or after Kairos.
pub fn boot_after_drivernet(
    inv: &PciInventory,
    drive: &DrivernetSummary,
    serial: &mut SerialPort,
) {
    // SAFETY: boulder_main is serialized on BSP with interrupts still
    // under explicit control at this phase; only caller of boot hook.
    let st = unsafe { state_mut() };
    *st = BlackLabState::empty();

    match seed_from_pci_and_drivernet(inv, drive, &mut st.quiver) {
        Ok(rep) => {
            let _ = writeln!(
                serial,
                "BlackLab: quiver seed devs={} strat={} arrows={}",
                rep.devices_kept, rep.strategy_nodes, rep.arrows
            );
        }
        Err(e) => {
            let _ = writeln!(serial, "BlackLab: quiver seed failed: {e:?}");
            return;
        }
    }

    st.faces = hodge_from_quiver(&st.quiver, &mut st.hodge);
    st.boot_energy = st.hodge.nonharmonic_energy0();
    let _ = writeln!(
        serial,
        "BlackLab: Hodge nerve V={} E={} F={} energy0={}",
        st.hodge.n_v, st.hodge.n_e, st.faces, st.boot_energy
    );

    // Prove δ₁δ₀=0 on seeded gradient if any face exists
    if st.hodge.n_f > 0 {
        st.hodge.store_gradient_flux();
        let mut beta = [0i32; crate::hodge_cech::MAX_F];
        st.hodge.coboundary1(&st.hodge.f1, &mut beta);
        let mut ok = true;
        for i in 0..st.hodge.n_f {
            if beta[i] != 0 {
                ok = false;
                break;
            }
        }
        let _ = writeln!(
            serial,
            "BlackLab: complex identity δ₁δ₀=0 {}",
            if ok { "ok" } else { "FAIL" }
        );
    }

    // Heat flow bring-up
    st.hodge.heat_flow0(HFP_ONE / 8, 16);
    let e1 = st.hodge.nonharmonic_energy0();
    let _ = writeln!(
        serial,
        "BlackLab: heat_flow energy0 {} -> {}",
        st.boot_energy, e1
    );

    // Cluster mutation on hottest
    match st.quiver.mutate_hottest(FP_ONE / 4) {
        Ok(Some(k)) => {
            st.last_mutated = Some(k);
            let _ = writeln!(
                serial,
                "BlackLab: cluster μ_{} x'={} arrows={}",
                k,
                st.quiver.x[k],
                st.quiver.live_arrows()
            );
        }
        Ok(None) => {
            let _ = writeln!(serial, "BlackLab: cluster no mutation (cool)");
        }
        Err(e) => {
            let _ = writeln!(serial, "BlackLab: cluster mutate err {e:?}");
        }
    }

    // Fair-Q n=64
    st.fairq = CyclotomicFairQ::new(16);
    // seed deficits from node kinds
    for i in 0..st.quiver.n.min(16) {
        let amt = (st.quiver.x[i] / FP_ONE).max(1);
        st.fairq.charge(i, amt);
    }
    let pick = st.fairq.quantum(1);
    let _ = writeln!(
        serial,
        "BlackLab: NTT64 fairq pick={} picks_total={}",
        pick, st.fairq.picks
    );

    let mut scales = [0u32; MAX_N];
    st.quiver.ceiling_scales(&mut scales);
    let _ = writeln!(
        serial,
        "BlackLab: ceiling scales[0..4]= {:#x} {:#x} {:#x} {:#x}",
        scales.get(0).copied().unwrap_or(0),
        scales.get(1).copied().unwrap_or(0),
        scales.get(2).copied().unwrap_or(0),
        scales.get(3).copied().unwrap_or(0),
    );

    READY.store(true, Ordering::Release);
    let _ = writeln!(serial, "BlackLab: boot hooks armed");
}

/// Periodic tick — call from APIC timer softpath when ready.
pub fn tick(_now_tsc: u64) -> Option<usize> {
    if !ready() {
        return None;
    }
    let st = unsafe { state_mut() };
    st.hodge.heat_step0(HFP_ONE / 16);
    // refresh loads from quiver mass
    for i in 0..st.quiver.n.min(st.hodge.n_v) {
        let _ = st.hodge.set_load(i, st.quiver.x[i] as i32);
    }
    let _ = st.quiver.mutate_hottest(2 * FP_ONE);
    Some(st.fairq.quantum(1))
}
