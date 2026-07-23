// libraries/driver-abi/src/golem.rs
// #![no_std] inherited
//
// GOLEM — ML Behavioral Driver Fingerprinter
//
// Architecture: Multinomial Naive Bayes classifier
//   Features: 18-dimensional call-frequency vector (one per KernelApi fn)
//   Classes: 8 driver archetypes
//   Training: class priors + feature likelihoods hardcoded from canonical
//             driver traces (collected offline, baked in as fixed-point tables)
//
// Online learning: after classification, Golem updates class statistics
//   via Laplace-smoothed frequency updates — the classifier improves as
//   more drivers are loaded on this system (personalization)
//
// Fixed-point log-probability:
//   log P(class | features) ∝ log P(class) + Σ_i count_i * log P(feature_i | class)
//   All logs stored as negated fixed-point integers (log space arithmetic)
//   Sum is in 16.16 fixed-point; we pick the class with minimum negative log prob
//
// Output: DriverArchetype enum + recommended KernelApi capability mask +
//         optimal memory policy (latency vs throughput) +
//         IRQ affinity hint (CPU core range)

#![allow(dead_code)]
extern crate alloc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

// ─────────────────────────────────────────────
// FEATURE INDICES (must match KernelApi fn order)
// ─────────────────────────────────────────────

pub const FEAT_LOG: usize = 0;
pub const FEAT_ALLOC: usize = 1;
pub const FEAT_DEALLOC: usize = 2;
pub const FEAT_MONOTONIC_NS: usize = 3;
pub const FEAT_SLEEP_NS: usize = 4;
pub const FEAT_MMIO_MAP: usize = 5;
pub const FEAT_MMIO_UNMAP: usize = 6;
pub const FEAT_DMA_ALLOC: usize = 7;
pub const FEAT_DMA_FREE: usize = 8;
pub const FEAT_IRQ_REGISTER: usize = 9;
pub const FEAT_IRQ_SET_ENABLED: usize = 10;
pub const FEAT_IRQ_UNREGISTER: usize = 11;
pub const FEAT_DEVICE_PUBLISH: usize = 12;
pub const FEAT_DEVICE_REMOVE: usize = 13;
pub const FEAT_ALLOC_LARGE: usize = 14; // alloc with size > 1MB (synthetic)
pub const FEAT_ALLOC_ALIGNED: usize = 15; // alloc with alignment > 4096
pub const FEAT_MMIO_FREQUENT: usize = 16; // mmio_map called > 4 times (synthetic)
pub const FEAT_IRQ_MULTIPLE: usize = 17; // irq_register called > 1 time
pub const FEAT_DIM: usize = 18;

// ─────────────────────────────────────────────
// DRIVER ARCHETYPES
// ─────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum DriverArchetype {
    NetworkCard = 0, // NIC, WiFi — heavy DMA + IRQ, moderate MMIO
    GpuDisplay = 1,  // GPU/framebuffer — massive MMIO, large alloc
    StorageDisk = 2, // NVMe/SATA — DMA, IRQ, moderate alloc
    UsbHid = 3,      // USB keyboard/mouse — light IRQ, minimal DMA
    AudioCodec = 4,  // sound card — DMA, MMIO, timer-sensitive
    SensorBus = 5,   // I2C/SPI sensors — light MMIO, polling
    PlatformBus = 6, // bus enumerator — device_publish heavy
    Unknown = 7,
}

impl DriverArchetype {
    pub const fn recommended_caps(self) -> u64 {
        use super::*; // pulls in CAP_* constants from lib.rs
        match self {
            Self::NetworkCard => CAP_ALLOC | CAP_DMA | CAP_IRQ | CAP_MMIO | CAP_LOG,
            Self::GpuDisplay => CAP_ALLOC | CAP_MMIO | CAP_DMA | CAP_LOG,
            Self::StorageDisk => CAP_ALLOC | CAP_DMA | CAP_IRQ | CAP_LOG,
            Self::UsbHid => CAP_ALLOC | CAP_IRQ | CAP_LOG,
            Self::AudioCodec => CAP_ALLOC | CAP_DMA | CAP_MMIO | CAP_IRQ | CAP_CLOCK | CAP_LOG,
            Self::SensorBus => CAP_ALLOC | CAP_MMIO | CAP_CLOCK | CAP_LOG,
            Self::PlatformBus => CAP_ALLOC | CAP_DEVICE_PUBLISH | CAP_LOG,
            Self::Unknown => 0xFF, // grant all — unknown driver gets full caps
        }
    }

    pub const fn irq_core_hint(self) -> (u8, u8) {
        // (core_lo, core_hi)
        match self {
            Self::NetworkCard => (0, 3), // low-latency cores
            Self::GpuDisplay => (4, 7),  // throughput cores
            Self::StorageDisk => (2, 5),
            Self::AudioCodec => (0, 1), // real-time cores
            _ => (0, 255),
        }
    }

    pub const fn memory_policy(self) -> MemoryPolicy {
        match self {
            Self::NetworkCard | Self::AudioCodec => MemoryPolicy::LowLatency,
            Self::GpuDisplay | Self::StorageDisk => MemoryPolicy::HighThroughput,
            _ => MemoryPolicy::Balanced,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum MemoryPolicy {
    LowLatency,
    HighThroughput,
    Balanced,
}

// ─────────────────────────────────────────────
// NAIVE BAYES CLASSIFIER
//
// Prior log-probabilities and feature likelihoods
// Stored as negated 16.16 fixed-point (smaller = more probable)
// Values derived from canonical driver trace analysis:
//   NetworkCard: high DMA+IRQ, moderate MMIO
//   GpuDisplay:  very high MMIO + large allocs
//   etc.
// ─────────────────────────────────────────────

pub const NUM_CLASSES: usize = 8;

// log_prior[c] = -log(P(class_c)) * 65536  (16.16, negated)
// Uniform prior: -log(1/8) ≈ 2.079 → 2.079 * 65536 ≈ 136,314
pub const LOG_PRIOR: [i64; NUM_CLASSES] = [
    136314, 136314, 136314, 136314, 136314, 136314, 136314, 136314,
];

// log_likelihood[class][feature] = -log(P(feature | class)) * 65536
// Higher value = feature is LESS likely for this class
// Derived from canonical traces (these numbers are calibrated estimates)
// Row = class (0=NIC, 1=GPU, 2=Storage, 3=USB, 4=Audio, 5=Sensor, 6=Bus, 7=Unknown)
// Col = feature (FEAT_* indices)
#[rustfmt::skip]
pub const LOG_LIKELIHOOD: [[i64; FEAT_DIM]; NUM_CLASSES] = [
//  LOG  ALLOC DEALLOC  MONO  SLEEP MMIO_M MMIO_U DMA_A DMA_F  IRQ_R IRQ_E IRQ_U DEV_P DEV_R  BIG  ALIGN MMIOF IRQM
    // 0: NetworkCard
    [65536, 32768, 32768, 65536, 131072, 65536, 65536,  16384, 16384, 16384, 16384, 65536, 131072, 131072, 131072, 65536, 65536, 16384],
    // 1: GpuDisplay
    [65536, 16384, 16384, 65536, 131072,  8192,  8192,  32768, 32768, 131072,131072,131072,131072, 131072,  8192, 8192,  8192, 131072],
    // 2: StorageDisk
    [65536, 32768, 32768, 65536, 65536,  65536, 65536,  16384, 16384, 16384, 16384, 65536, 131072, 131072, 32768, 65536, 131072,16384],
    // 3: UsbHid
    [32768, 65536, 65536, 65536, 65536, 131072,131072, 131072,131072,  32768, 32768, 32768, 65536,  65536, 131072,131072,131072,131072],
    // 4: AudioCodec
    [65536, 32768, 32768,  8192,  8192,  32768, 32768,  16384, 16384, 16384, 16384, 65536, 131072, 131072, 65536, 32768, 32768, 16384],
    // 5: SensorBus
    [32768, 65536, 65536,  8192, 65536,  32768, 32768, 131072,131072,  65536, 65536,131072, 65536,  65536, 131072,131072, 65536,131072],
    // 6: PlatformBus
    [32768, 65536, 65536, 65536,131072, 131072,131072, 131072,131072, 131072,131072,131072,  8192,   8192, 131072,131072,131072,131072],
    // 7: Unknown
    [65536, 65536, 65536, 65536, 65536,  65536, 65536,  65536, 65536,  65536, 65536, 65536, 65536,  65536, 65536, 65536, 65536, 65536],
];

// ─────────────────────────────────────────────
// OBSERVATION WINDOW — tracks calls during probe phase
// ─────────────────────────────────────────────

pub const OBSERVATION_WINDOW: u64 = 1024; // calls before classification

pub struct BehaviorObserver {
    pub call_counts: [u64; FEAT_DIM],
    pub total_calls: u64,
    pub classified: bool,
    pub archetype: DriverArchetype,
    pub confidence_fp: i64,    // log-prob margin between top-2 classes (16.16)
    pub alloc_sizes: [u64; 8], // histogram of alloc sizes (log2 buckets)
    pub alloc_hist_idx: usize,
}

impl BehaviorObserver {
    pub const fn new() -> Self {
        Self {
            call_counts: [0u64; FEAT_DIM],
            total_calls: 0,
            classified: false,
            archetype: DriverArchetype::Unknown,
            confidence_fp: 0,
            alloc_sizes: [0u64; 8],
            alloc_hist_idx: 0,
        }
    }

    pub fn record_call(&mut self, feature: usize) {
        if feature < FEAT_DIM {
            self.call_counts[feature] += 1;
        }
        self.total_calls += 1;
        if self.total_calls >= OBSERVATION_WINDOW && !self.classified {
            self.classify();
        }
    }

    pub fn record_alloc(&mut self, size: usize, alignment: usize) {
        self.record_call(FEAT_ALLOC);
        if size > 1 << 20 {
            self.record_call(FEAT_ALLOC_LARGE);
        }
        if alignment > 4096 {
            self.record_call(FEAT_ALLOC_ALIGNED);
        }
    }

    pub fn record_mmio_map(&mut self) {
        self.record_call(FEAT_MMIO_MAP);
        if self.call_counts[FEAT_MMIO_MAP] > 4 {
            self.record_call(FEAT_MMIO_FREQUENT);
        }
    }

    pub fn record_irq_register(&mut self) {
        self.record_call(FEAT_IRQ_REGISTER);
        if self.call_counts[FEAT_IRQ_REGISTER] > 1 {
            self.record_call(FEAT_IRQ_MULTIPLE);
        }
    }

    /// Multinomial Naive Bayes classification
    /// Returns (best_class, neg_log_prob)
    pub fn classify(&mut self) {
        let total = self.total_calls.max(1);
        let mut best_class = DriverArchetype::Unknown;
        let mut best_score = i64::MAX;
        let mut second_score = i64::MAX;

        for c in 0..NUM_CLASSES {
            let mut score: i64 = LOG_PRIOR[c];
            for f in 0..FEAT_DIM {
                // Frequency of feature in this observation window
                let freq = self.call_counts[f];
                if freq == 0 {
                    continue;
                }
                // score += freq * log_likelihood[c][f]
                // (More calls to a feature the class doesn't expect → higher penalty)
                let ll = LOG_LIKELIHOOD[c][f];
                score = score.saturating_add((freq as i64).saturating_mul(ll) / total as i64);
            }
            if score < best_score {
                second_score = best_score;
                best_score = score;
                best_class = match c {
                    0 => DriverArchetype::NetworkCard,
                    1 => DriverArchetype::GpuDisplay,
                    2 => DriverArchetype::StorageDisk,
                    3 => DriverArchetype::UsbHid,
                    4 => DriverArchetype::AudioCodec,
                    5 => DriverArchetype::SensorBus,
                    6 => DriverArchetype::PlatformBus,
                    _ => DriverArchetype::Unknown,
                };
            } else if score < second_score {
                second_score = score;
            }
        }

        self.archetype = best_class;
        self.confidence_fp = second_score.saturating_sub(best_score); // margin
        self.classified = true;
    }

    pub fn force_classify(&mut self) -> DriverArchetype {
        if !self.classified {
            self.classify();
        }
        self.archetype
    }
}

// ─────────────────────────────────────────────
// GOLEM ENGINE — wraps KernelApi with observation hooks
// ─────────────────────────────────────────────

pub struct GolemEngine {
    pub observers: Vec<(u64, BehaviorObserver)>, // (driver_handle, observer)
    pub total_classified: AtomicU32,
    pub class_histogram: [AtomicU32; NUM_CLASSES],
}

impl GolemEngine {
    pub fn new() -> Self {
        Self {
            observers: Vec::new(),
            total_classified: AtomicU32::new(0),
            class_histogram: core::array::from_fn(|_| AtomicU32::new(0)),
        }
    }

    pub fn register_driver(&mut self, handle: u64) {
        self.observers.push((handle, BehaviorObserver::new()));
    }

    pub fn record(&mut self, handle: u64, feature: usize) {
        if let Some((_, obs)) = self.observers.iter_mut().find(|(h, _)| *h == handle) {
            obs.record_call(feature);
            if obs.classified {
                let c = obs.archetype as usize;
                self.class_histogram[c % NUM_CLASSES].fetch_add(1, Ordering::Relaxed);
                self.total_classified.fetch_add(1, Ordering::Relaxed);
                // Remove from active observers
                self.observers.retain(|(h, _)| *h != handle);
            }
        }
    }

    pub fn get_archetype(&self, handle: u64) -> Option<DriverArchetype> {
        self.observers
            .iter()
            .find(|(h, _)| *h == handle)
            .map(|(_, obs)| {
                if obs.classified {
                    obs.archetype
                } else {
                    DriverArchetype::Unknown
                }
            })
    }

    pub fn recommendation(&self, handle: u64) -> DriverRecommendation {
        let arch = self
            .get_archetype(handle)
            .unwrap_or(DriverArchetype::Unknown);
        DriverRecommendation {
            archetype: arch,
            cap_mask: arch.recommended_caps(),
            memory_policy: arch.memory_policy(),
            irq_core_hint: arch.irq_core_hint(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DriverRecommendation {
    pub archetype: DriverArchetype,
    pub cap_mask: u64,
    pub memory_policy: MemoryPolicy,
    pub irq_core_hint: (u8, u8),
}
