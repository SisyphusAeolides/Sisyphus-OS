// kernel/boulder/src/ipc/macrophage.rs
// #![no_std] inherited
//
// MACROPHAGE — Biological IPC Firewall & Immune System
//
// Rather than traditional static firewall rules, the kernel deploys "Macrophages"
// (White Blood Cells) that patrol the IPC channels.
//
// Pathogens: Malicious or malformed IPC messages have high entropy or 
// mismatched semantic hashes.
// Phagocytosis: When a Macrophage encounters a pathogen, it "eats" it, 
// neutralizing the message before it reaches the receiver.
// Antigen Presentation: The Macrophage extracts a signature (Antigen) from 
// the consumed message and broadcasts it. Other Macrophages build immunity 
// against this sender, escalating the sender's threat level.
// Apoptosis: If a sender's threat level exceeds a threshold, the kernel 
// induces apoptosis (programmed cell death) in the sending process.

#![allow(dead_code)]
extern crate alloc;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU32, Ordering};

pub const THREAT_APOPTOSIS_THRESHOLD: u32 = 100;
pub const MAX_MACROPHAGES: usize = 32;

pub struct Antigen {
    pub signature_hash: u64,
    pub sender_pid:     u32,
    pub threat_score:   u32,
}

pub struct Macrophage {
    pub id: u32,
    pub current_ring: u32,       // ID of the IPC ring it is currently patrolling
    pub memory_t_cells: Vec<u64>, // Known pathogen signatures
    pub phagocytized_count: u32,
}

impl Macrophage {
    pub fn new(id: u32, ring: u32) -> Self {
        Self {
            id,
            current_ring: ring,
            memory_t_cells: Vec::new(),
            phagocytized_count: 0,
        }
    }

    /// Inspect a message. Returns an Antigen if it's considered malicious.
    pub fn inspect(&mut self, sender: u32, payload_hash: u64, payload_len: usize) -> Option<Antigen> {
        // Fast path: known pathogen
        if self.memory_t_cells.contains(&payload_hash) {
            self.phagocytized_count += 1;
            return Some(Antigen {
                signature_hash: payload_hash,
                sender_pid: sender,
                threat_score: 20, // High threat for repeat offenses
            });
        }

        // Heuristic: Overly large messages or specific bit patterns 
        // (In a real system this would use entropy analysis or neural nets)
        if payload_len > 1_000_000 {
            self.phagocytized_count += 1;
            let mut threat = (payload_len / 100_000) as u32;
            if threat > 50 { threat = 50; }
            return Some(Antigen {
                signature_hash: payload_hash,
                sender_pid: sender,
                threat_score: threat,
            });
        }
        
        None
    }

    pub fn learn_antigen(&mut self, antigen: &Antigen) {
        if !self.memory_t_cells.contains(&antigen.signature_hash) {
            self.memory_t_cells.push(antigen.signature_hash);
            // Cap memory size to simulate biological forgetting/turnover
            if self.memory_t_cells.len() > 1024 {
                self.memory_t_cells.remove(0);
            }
        }
    }
}

pub struct ImmuneSystem {
    pub macrophages: Vec<Macrophage>,
    pub process_threat_levels: BTreeMap<u32, u32>,
    pub apoptosis_signals: Vec<u32>, // PIDs to kill
    pub total_neutralized: AtomicU32,
}

impl ImmuneSystem {
    pub fn new() -> Self {
        let mut sys = Self {
            macrophages: Vec::new(),
            process_threat_levels: BTreeMap::new(),
            apoptosis_signals: Vec::new(),
            total_neutralized: AtomicU32::new(0),
        };
        // Deploy initial squad of macrophages
        for i in 0..MAX_MACROPHAGES {
            sys.macrophages.push(Macrophage::new(i as u32, 0));
        }
        sys
    }

    pub fn patrol_ipc(&mut self, ring_id: u32, sender: u32, payload_hash: u64, payload_len: usize) -> bool {
        let mut found_antigen = None;
        
        for m in &mut self.macrophages {
            if m.current_ring == ring_id || m.current_ring == 0 /* 0 means all rings */ {
                if let Some(antigen) = m.inspect(sender, payload_hash, payload_len) {
                    found_antigen = Some(antigen);
                    self.total_neutralized.fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
        }

        if let Some(antigen) = found_antigen {
            self.broadcast_antigen(&antigen);
            self.escalate_threat(antigen.sender_pid, antigen.threat_score);
            return true; // Message was eaten!
        }
        
        false // Message is safe
    }

    fn broadcast_antigen(&mut self, antigen: &Antigen) {
        for m in &mut self.macrophages {
            m.learn_antigen(antigen);
        }
    }

    fn escalate_threat(&mut self, pid: u32, score: u32) {
        let threat = self.process_threat_levels.entry(pid).or_insert(0);
        *threat = threat.saturating_add(score);

        if *threat >= THREAT_APOPTOSIS_THRESHOLD && !self.apoptosis_signals.contains(&pid) {
            self.apoptosis_signals.push(pid);
        }
    }

    pub fn drain_apoptosis_signals(&mut self) -> Vec<u32> {
        let mut signals = Vec::new();
        core::mem::swap(&mut self.apoptosis_signals, &mut signals);
        signals
    }
}
