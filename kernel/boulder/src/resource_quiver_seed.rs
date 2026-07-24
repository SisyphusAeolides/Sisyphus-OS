// kernel/boulder/src/resource_quiver_seed.rs
//! Seed a resource quiver from PciInventory + DrivernetSummary
//!
//! Node layout (compact, ≤ MAX_N):
//!   0 .. n_dev-1     one node per selected PCI function (by class priority)
//!   n_dev .. +strat  one node per armed DriverStrategy (unique)
//!   last-1           DMA pool aggregate
//!   last             IRQ budget aggregate
//!
//! Arrows:
//!   bridge (class 0x06) → every non-bridge on same bus
//!   device → its strategy node (if display / resolved)
//!   every device → DMA pool (weight by BAR-ish class)
//!   every device with pin≠0 → IRQ budget
//!
//! Cluster x[i] seeded from class heuristics; congestion from strategy rank.


use crate::cluster_quiver::{ClusterFault, FP_ONE, Fp, MAX_N, NodeKind, ResourceQuiver};
use crate::drivers::drivernet::DriverNetSummary;
use crate::drivers::drivernet::model::DriverStrategy;
use crate::hw::pci::{PciDevice, PciInventory};

const MAX_SEED_DEV: usize = 10;

#[derive(Clone, Copy, Debug)]
pub struct SeedReport {
    pub devices_kept: u8,
    pub strategy_nodes: u8,
    pub arrows: u16,
    pub faces_filled: u16,
}

fn class_priority(d: &PciDevice) -> u8 {
    match d.class_code {
        0x03 => 0, // display first
        0x02 => 1, // network
        0x01 => 2, // storage
        0x0c => 3, // serial bus
        0x06 => 4, // bridge
        _ => 5,
    }
}

fn kind_of(d: &PciDevice) -> NodeKind {
    match d.class_code {
        0x03 => NodeKind::Display,
        0x02 => NodeKind::Network,
        0x01 => NodeKind::Storage,
        0x0c => NodeKind::Usb,
        0x06 => NodeKind::Bridge,
        _ => NodeKind::Other,
    }
}

fn seed_x(d: &PciDevice) -> Fp {
    match d.class_code {
        0x03 => 4 * FP_ONE,
        0x02 => 3 * FP_ONE,
        0x01 => 3 * FP_ONE,
        0x06 => 2 * FP_ONE,
        _ => FP_ONE,
    }
}

fn strategy_congestion(s: DriverStrategy) -> Fp {
    match s {
        DriverStrategy::HermesNvidia => 3 * FP_ONE,
        DriverStrategy::AmdDisplay | DriverStrategy::IntelDisplay => 2 * FP_ONE,
        DriverStrategy::VirtioGpu | DriverStrategy::VirtualSvga => FP_ONE,
        DriverStrategy::FirmwareFramebuffer => FP_ONE / 2,
        DriverStrategy::Quarantine => 4 * FP_ONE,
    }
}

/// Build quiver. Returns report. `q` is overwritten.
pub fn seed_from_pci_and_drivernet(
    inv: &PciInventory,
    drive: &DriverNetSummary,
    q: &mut ResourceQuiver,
) -> Result<SeedReport, ClusterFault> {
    let devices = inv.devices();
    // pick top devices by priority
    let mut idx = [0u16; MAX_SEED_DEV];
    let mut n_keep = 0usize;
    for (i, d) in devices.iter().enumerate() {
        if n_keep < MAX_SEED_DEV {
            idx[n_keep] = i as u16;
            n_keep += 1;
            // insertion sort by priority
            let mut j = n_keep - 1;
            while j > 0
                && class_priority(&devices[idx[j] as usize])
                    < class_priority(&devices[idx[j - 1] as usize])
            {
                idx.swap(j, j - 1);
                j -= 1;
            }
        } else {
            // replace worst if better
            let worst = n_keep - 1;
            if class_priority(d) < class_priority(&devices[idx[worst] as usize]) {
                idx[worst] = i as u16;
                let mut j = worst;
                while j > 0
                    && class_priority(&devices[idx[j] as usize])
                        < class_priority(&devices[idx[j - 1] as usize])
                {
                    idx.swap(j, j - 1);
                    j -= 1;
                }
            }
        }
    }

    // unique strategies from drivernet
    let mut strats = [DriverStrategy::FirmwareFramebuffer; 5];
    let mut n_strat = 0usize;
    for r in drive.resolutions() {
        let s = r.strategy;
        if !strats[..n_strat].iter().any(|&t| t == s) && n_strat < 5 {
            strats[n_strat] = s;
            n_strat += 1;
        }
    }

    let dma_i = n_keep + n_strat;
    let irq_i = dma_i + 1;
    let total = irq_i + 1;
    if total > MAX_N {
        return Err(ClusterFault::Dim);
    }
    *q = ResourceQuiver::new(total);

    for (local, &di) in idx.iter().take(n_keep).enumerate() {
        let d = &devices[di as usize];
        q.set_node(local, kind_of(d), di, seed_x(d))?;
        q.set_congestion(local, FP_ONE)?;
    }
    for s in 0..n_strat {
        let ni = n_keep + s;
        q.set_node(ni, NodeKind::Strategy, strats[s] as u16, 2 * FP_ONE)?;
        q.set_congestion(ni, strategy_congestion(strats[s]))?;
    }
    q.set_node(dma_i, NodeKind::DmaPool, 0xFF01, 8 * FP_ONE)?;
    q.set_node(irq_i, NodeKind::IrqBudget, 0xFF02, 8 * FP_ONE)?;
    q.set_congestion(dma_i, FP_ONE)?;
    q.set_congestion(irq_i, FP_ONE)?;

    // bridges → same-bus devices
    for a in 0..n_keep {
        let da = &devices[idx[a] as usize];
        if da.class_code != 0x06 {
            continue;
        }
        for b in 0..n_keep {
            if a == b {
                continue;
            }
            let db = &devices[idx[b] as usize];
            if da.address.bus == db.address.bus && db.class_code != 0x06 {
                let _ = q.add_arrow(a as u8, b as u8, 1);
            }
        }
    }

    // device → strategy (match by vendor path through resolutions)
    for a in 0..n_keep {
        let da = &devices[idx[a] as usize];
        if da.class_code != 0x03 {
            continue;
        }
        for r in drive.resolutions() {
            // fingerprint carries vendor/device — match loosely on class display
            let s = r.strategy;
            if let Some(si) = strats[..n_strat].iter().position(|&t| t == s) {
                let _ = q.add_arrow(a as u8, (n_keep + si) as u8, 1);
            }
        }
    }

    // devices → DMA + IRQ
    for a in 0..n_keep {
        let d = &devices[idx[a] as usize];
        let _ = q.add_arrow(a as u8, dma_i as u8, 1);
        if d.interrupt_pin != 0 {
            let _ = q.add_arrow(a as u8, irq_i as u8, 1);
        }
    }
    // strategies feed DMA
    for s in 0..n_strat {
        let _ = q.add_arrow((n_keep + s) as u8, dma_i as u8, 1);
    }

    Ok(SeedReport {
        devices_kept: n_keep as u8,
        strategy_nodes: n_strat as u8,
        arrows: q.live_arrows() as u16,
        faces_filled: 0,
    })
}

/// Build Hodge nerve vertices = quiver nodes, edges = quiver arrows, fill triangles.
pub fn hodge_from_quiver(q: &ResourceQuiver, h: &mut crate::hodge_cech::HodgeNerve) -> u16 {
    *h = crate::hodge_cech::HodgeNerve::new(q.n);
    for i in 0..q.n {
        let _ = h.set_load(i, q.x[i] as i32);
    }
    for a in q.arrows.iter().take(q.e_len) {
        if a.live {
            let _ = h.add_edge(a.from, a.to, a.mult as u16);
        }
    }
    h.fill_clique_triangles() as u16
}
