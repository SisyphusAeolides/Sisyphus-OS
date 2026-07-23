use super::fingerprint::{
    CAP_MSI, CAP_MSIX, CAP_PCIE, GpuFingerprint, TOPOLOGY_BOOT_DISPLAY, TOPOLOGY_CONFIG_INCOMPLETE,
    TOPOLOGY_FIRMWARE_FRAMEBUFFER, TOPOLOGY_HOTPLUG_PORT, TOPOLOGY_INTERNAL_PANEL,
    TOPOLOGY_INVENTORY_OVERFLOW, TOPOLOGY_IOMMU_PRESENT, TOPOLOGY_REMOVABLE,
    TOPOLOGY_VIRTUAL_MACHINE, VENDOR_AMD, VENDOR_BOCHS, VENDOR_INTEL, VENDOR_NVIDIA, VENDOR_REDHAT,
    VENDOR_VIRTIO, VENDOR_VMWARE,
};

use super::model_weights::{
    BIAS, FEATURE_CENTER, FEATURE_COUNT, FEATURE_SCALE, MINIMUM_MARGIN, MODEL_CORPUS_ROOT,
    MODEL_ROOT, MODEL_SCHEMA_VERSION, STRATEGY_COUNT, WEIGHTS,
};

pub const MAXIMUM_RANKED_CANDIDATES: usize = 7;
pub const FEATURE_Q: i32 = 256;

pub const REASON_LOW_MARGIN: u32 = 1 << 0;
pub const REASON_FIRMWARE_PRESERVED: u32 = 1 << 1;
pub const REASON_INVENTORY_INCOMPLETE: u32 = 1 << 2;
pub const REASON_IOMMU_ABSENT: u32 = 1 << 3;
pub const REASON_UNKNOWN_VENDOR: u32 = 1 << 4;
pub const REASON_BOOT_DISPLAY: u32 = 1 << 5;
pub const REASON_VIRTUAL_DEVICE: u32 = 1 << 6;
pub const REASON_NATIVE_GATED: u32 = 1 << 7;
pub const REASON_CONFIG_INCOMPLETE: u32 = 1 << 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelExpectation {
    pub schema: u32,
    pub corpus_root: u64,
    pub model_root: u64,
}

impl ModelExpectation {
    pub const COMPILED: Self = Self {
        schema: MODEL_SCHEMA_VERSION,
        corpus_root: MODEL_CORPUS_ROOT,
        model_root: MODEL_ROOT,
    };

    pub const fn matches_compiled(self) -> bool {
        self.schema == MODEL_SCHEMA_VERSION
            && self.corpus_root == MODEL_CORPUS_ROOT
            && self.model_root == MODEL_ROOT
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum DriverStrategy {
    HermesNvidia = 0,
    AmdDisplay = 1,
    IntelDisplay = 2,
    VirtioGpu = 3,
    VirtualSvga = 4,
    FirmwareFramebuffer = 5,
    Quarantine = 6,
}

impl DriverStrategy {
    pub const ALL: [Self; STRATEGY_COUNT] = [
        Self::HermesNvidia,
        Self::AmdDisplay,
        Self::IntelDisplay,
        Self::VirtioGpu,
        Self::VirtualSvga,
        Self::FirmwareFramebuffer,
        Self::Quarantine,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::HermesNvidia => "hermes-nvidia",
            Self::AmdDisplay => "amd-display",
            Self::IntelDisplay => "intel-display",
            Self::VirtioGpu => "virtio-gpu",
            Self::VirtualSvga => "virtual-svga",
            Self::FirmwareFramebuffer => "firmware-framebuffer",
            Self::Quarantine => "quarantine",
        }
    }

    pub const fn native(self) -> bool {
        matches!(
            self,
            Self::HermesNvidia
                | Self::AmdDisplay
                | Self::IntelDisplay
                | Self::VirtioGpu
                | Self::VirtualSvga
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeatureVector {
    pub values: [i16; FEATURE_COUNT],
    pub known_mask: u32,
    pub root: u64,
}

impl FeatureVector {
    pub const ZERO: Self = Self {
        values: [0; FEATURE_COUNT],
        known_mask: 0,
        root: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RankedCandidate {
    pub strategy: DriverStrategy,
    pub score: i64,
    pub margin: i64,
    pub admissible: bool,
    pub gate_reason: u32,
}

impl RankedCandidate {
    pub const EMPTY: Self = Self {
        strategy: DriverStrategy::Quarantine,
        score: i64::MIN,
        margin: 0,
        admissible: false,
        gate_reason: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OracleDecision {
    pub fingerprint_root: u64,
    pub feature_root: u64,
    pub model_schema: u32,
    pub model_corpus_root: u64,
    pub model_root: u64,
    pub ranked: [RankedCandidate; MAXIMUM_RANKED_CANDIDATES],
    pub length: usize,
    pub best_margin: i64,
    pub confidence_q16: u16,
    pub reasons: u32,
    pub decision_root: u64,
}

impl OracleDecision {
    pub fn candidates(&self) -> &[RankedCandidate] {
        &self.ranked[..self.length]
    }

    pub fn verify(&self, secret: u64) -> bool {
        self.length <= self.ranked.len()
            && self.model_schema == MODEL_SCHEMA_VERSION
            && self.model_corpus_root == MODEL_CORPUS_ROOT
            && self.model_root == MODEL_ROOT
            && self.decision_root == decision_root(secret, self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OraclePolicy {
    pub require_iommu_for_native: bool,
    pub preserve_boot_framebuffer: bool,
    pub minimum_confidence_q16: u16,
    pub allow_virtual_devices: bool,
    pub quarantine_on_inventory_overflow: bool,
}

impl OraclePolicy {
    pub const BLACK_LAB: Self = Self {
        require_iommu_for_native: true,
        preserve_boot_framebuffer: true,
        minimum_confidence_q16: 12_000,
        allow_virtual_devices: true,
        quarantine_on_inventory_overflow: true,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OracleError {
    InvalidSecret,
    InvalidFingerprint,
    ModelShape,
    ModelAuthority,
    Arithmetic,
}

pub struct CompatibilityOracle {
    secret: u64,
    policy: OraclePolicy,
}

impl CompatibilityOracle {
    pub fn new(secret: u64, policy: OraclePolicy) -> Result<Self, OracleError> {
        Self::new_with_expectation(secret, policy, ModelExpectation::COMPILED)
    }

    pub fn new_with_expectation(
        secret: u64,
        policy: OraclePolicy,
        expectation: ModelExpectation,
    ) -> Result<Self, OracleError> {
        if secret == 0 {
            return Err(OracleError::InvalidSecret);
        }
        if WEIGHTS.len() != STRATEGY_COUNT
            || BIAS.len() != STRATEGY_COUNT
            || FEATURE_CENTER.len() != FEATURE_COUNT
            || FEATURE_SCALE.len() != FEATURE_COUNT
        {
            return Err(OracleError::ModelShape);
        }
        if !expectation.matches_compiled() {
            return Err(OracleError::ModelAuthority);
        }
        Ok(Self { secret, policy })
    }

    pub const fn policy(&self) -> OraclePolicy {
        self.policy
    }

    pub fn classify(&self, fingerprint: &GpuFingerprint) -> Result<OracleDecision, OracleError> {
        if fingerprint.evidence_root == 0 {
            return Err(OracleError::InvalidFingerprint);
        }

        let features = encode_features(fingerprint);
        let mut ranked = [RankedCandidate::EMPTY; MAXIMUM_RANKED_CANDIDATES];

        for strategy in DriverStrategy::ALL {
            let score = score_strategy(strategy, &features)?;
            let (admissible, gate_reason) = gate_strategy(strategy, fingerprint, self.policy);
            ranked[strategy.index()] = RankedCandidate {
                strategy,
                score,
                margin: 0,
                admissible,
                gate_reason,
            };
        }

        ranked.sort_unstable_by(|left, right| {
            right
                .admissible
                .cmp(&left.admissible)
                .then_with(|| right.score.cmp(&left.score))
                .then_with(|| left.strategy.index().cmp(&right.strategy.index()))
        });
        order_terminal_fallbacks(&mut ranked, fingerprint);

        let best_score = ranked
            .iter()
            .find(|candidate| candidate.admissible)
            .map(|candidate| candidate.score)
            .unwrap_or(i64::MIN);
        let second_score = ranked
            .iter()
            .filter(|candidate| candidate.admissible)
            .nth(1)
            .map(|candidate| candidate.score)
            .unwrap_or(i64::MIN);

        let best_margin = if second_score == i64::MIN {
            i64::MAX
        } else {
            best_score.saturating_sub(second_score)
        };

        for candidate in &mut ranked {
            candidate.margin = if candidate.admissible {
                candidate.score.saturating_sub(second_score)
            } else {
                i64::MIN
            };
        }

        let mut reasons = base_reasons(fingerprint, self.policy);
        let confidence_q16 = confidence_from_margin(best_margin, ranked[0].strategy);

        if confidence_q16 < self.policy.minimum_confidence_q16 {
            reasons |= REASON_LOW_MARGIN;
            promote_safe_fallback(&mut ranked, fingerprint);
        }

        if self.policy.preserve_boot_framebuffer
            && fingerprint.boot_display()
            && fingerprint.firmware_display_usable()
        {
            reasons |= REASON_FIRMWARE_PRESERVED | REASON_BOOT_DISPLAY;
        }

        let mut decision = OracleDecision {
            fingerprint_root: fingerprint.evidence_root,
            feature_root: features.root,
            model_schema: MODEL_SCHEMA_VERSION,
            model_corpus_root: MODEL_CORPUS_ROOT,
            model_root: MODEL_ROOT,
            ranked,
            length: MAXIMUM_RANKED_CANDIDATES,
            best_margin,
            confidence_q16,
            reasons,
            decision_root: 0,
        };
        decision.decision_root = decision_root(self.secret, &decision);
        Ok(decision)
    }
}

pub fn encode_features(fingerprint: &GpuFingerprint) -> FeatureVector {
    let mut values = [0_i16; FEATURE_COUNT];
    let mut known_mask = 0_u32;

    set_feature(
        &mut values,
        &mut known_mask,
        0,
        vendor_bucket(fingerprint.vendor_id),
        true,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        1,
        bucket_u16(fingerprint.device_id),
        fingerprint.device_id != 0xffff,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        2,
        bucket_u16(fingerprint.subsystem_vendor_id),
        fingerprint.subsystem_vendor_id != 0xffff,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        3,
        bucket_u16(fingerprint.subsystem_device_id),
        fingerprint.subsystem_device_id != 0xffff,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        4,
        i16::from(fingerprint.revision),
        true,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        5,
        i16::from(fingerprint.subclass) * 64,
        true,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        6,
        i16::from(fingerprint.programming_interface) * 64,
        true,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        7,
        capability_density(fingerprint.capability_flags),
        true,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        8,
        bool_feature(fingerprint.capability_flags & CAP_MSI != 0),
        true,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        9,
        bool_feature(fingerprint.capability_flags & CAP_MSIX != 0),
        true,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        10,
        bool_feature(fingerprint.capability_flags & CAP_PCIE != 0),
        true,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        11,
        bar_count(fingerprint),
        true,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        12,
        mmio64_count(fingerprint),
        true,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        13,
        prefetchable_count(fingerprint),
        true,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        14,
        resource_tier(fingerprint.total_declared_resources()),
        fingerprint.total_declared_resources() != 0,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        15,
        bool_feature(fingerprint.interrupt_pin != 0),
        true,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        16,
        bool_feature(fingerprint.boot_display()),
        true,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        17,
        bool_feature(fingerprint.topology_flags & TOPOLOGY_INTERNAL_PANEL != 0),
        true,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        18,
        bool_feature(fingerprint.topology_flags & TOPOLOGY_REMOVABLE != 0),
        true,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        19,
        bool_feature(fingerprint.topology_flags & TOPOLOGY_IOMMU_PRESENT != 0),
        true,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        20,
        bool_feature(fingerprint.iommu_isolated()),
        true,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        21,
        bool_feature(fingerprint.firmware_display_usable()),
        true,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        22,
        i16::from(fingerprint.root_port_depth) * 32,
        true,
    );
    set_feature(
        &mut values,
        &mut known_mask,
        23,
        i16::from(fingerprint.sibling_display_functions) * 64,
        true,
    );

    normalize(&mut values);

    FeatureVector {
        values,
        known_mask,
        root: feature_root(fingerprint.evidence_root, &values, known_mask),
    }
}

fn score_strategy(strategy: DriverStrategy, features: &FeatureVector) -> Result<i64, OracleError> {
    let index = strategy.index();
    let mut score = i64::from(BIAS[index]);

    for feature in 0..FEATURE_COUNT {
        if features.known_mask & (1_u32 << feature) == 0 {
            continue;
        }

        let product = i64::from(features.values[feature])
            .checked_mul(i64::from(WEIGHTS[index][feature]))
            .ok_or(OracleError::Arithmetic)?;
        score = score.checked_add(product).ok_or(OracleError::Arithmetic)?;
    }

    Ok(score)
}

fn gate_strategy(
    strategy: DriverStrategy,
    fingerprint: &GpuFingerprint,
    policy: OraclePolicy,
) -> (bool, u32) {
    if policy.quarantine_on_inventory_overflow
        && fingerprint.topology_flags & TOPOLOGY_INVENTORY_OVERFLOW != 0
        && strategy != DriverStrategy::FirmwareFramebuffer
        && strategy != DriverStrategy::Quarantine
    {
        return (false, REASON_INVENTORY_INCOMPLETE);
    }

    if policy.require_iommu_for_native && strategy.native() && !fingerprint.iommu_isolated() {
        return (false, REASON_IOMMU_ABSENT | REASON_NATIVE_GATED);
    }

    if strategy.native() && fingerprint.topology_flags & TOPOLOGY_CONFIG_INCOMPLETE != 0 {
        return (false, REASON_CONFIG_INCOMPLETE | REASON_NATIVE_GATED);
    }

    let vendor_gate = match strategy {
        DriverStrategy::HermesNvidia => {
            fingerprint.vendor_id == VENDOR_NVIDIA && fingerprint.has_mmio()
        }
        DriverStrategy::AmdDisplay => fingerprint.vendor_id == VENDOR_AMD && fingerprint.has_mmio(),
        DriverStrategy::IntelDisplay => {
            fingerprint.vendor_id == VENDOR_INTEL && fingerprint.has_mmio()
        }
        DriverStrategy::VirtioGpu => {
            policy.allow_virtual_devices
                && fingerprint.vendor_id == VENDOR_VIRTIO
                && fingerprint.has_mmio()
        }
        DriverStrategy::VirtualSvga => {
            policy.allow_virtual_devices
                && matches!(
                    fingerprint.vendor_id,
                    VENDOR_VMWARE | VENDOR_REDHAT | VENDOR_BOCHS
                )
        }
        DriverStrategy::FirmwareFramebuffer => fingerprint.firmware_display_usable(),
        DriverStrategy::Quarantine => true,
    };

    if vendor_gate {
        (true, 0)
    } else {
        (false, REASON_UNKNOWN_VENDOR)
    }
}

fn order_terminal_fallbacks(
    ranked: &mut [RankedCandidate; MAXIMUM_RANKED_CANDIDATES],
    fingerprint: &GpuFingerprint,
) {
    if !fingerprint.firmware_display_usable() {
        return;
    }

    let firmware = ranked.iter().position(|candidate| {
        candidate.admissible && candidate.strategy == DriverStrategy::FirmwareFramebuffer
    });
    let quarantine = ranked.iter().position(|candidate| {
        candidate.admissible && candidate.strategy == DriverStrategy::Quarantine
    });

    if let (Some(firmware), Some(quarantine)) = (firmware, quarantine) {
        if quarantine < firmware {
            ranked[quarantine..=firmware].rotate_right(1);
        }
    }
}

fn promote_safe_fallback(
    ranked: &mut [RankedCandidate; MAXIMUM_RANKED_CANDIDATES],
    fingerprint: &GpuFingerprint,
) {
    let strategy = if fingerprint.firmware_display_usable() {
        DriverStrategy::FirmwareFramebuffer
    } else {
        DriverStrategy::Quarantine
    };

    if let Some(index) = ranked
        .iter()
        .position(|candidate| candidate.strategy == strategy)
    {
        ranked[..=index].rotate_right(1);
    }
}

fn base_reasons(fingerprint: &GpuFingerprint, policy: OraclePolicy) -> u32 {
    let mut reasons = 0_u32;

    if fingerprint.topology_flags & TOPOLOGY_INVENTORY_OVERFLOW != 0 {
        reasons |= REASON_INVENTORY_INCOMPLETE;
    }
    if fingerprint.topology_flags & TOPOLOGY_CONFIG_INCOMPLETE != 0 {
        reasons |= REASON_CONFIG_INCOMPLETE;
    }
    if policy.require_iommu_for_native && !fingerprint.iommu_isolated() {
        reasons |= REASON_IOMMU_ABSENT;
    }
    if fingerprint.topology_flags & TOPOLOGY_VIRTUAL_MACHINE != 0
        || matches!(
            fingerprint.vendor_id,
            VENDOR_VIRTIO | VENDOR_VMWARE | VENDOR_REDHAT | VENDOR_BOCHS
        )
    {
        reasons |= REASON_VIRTUAL_DEVICE;
    }
    if fingerprint.topology_flags & TOPOLOGY_BOOT_DISPLAY != 0 {
        reasons |= REASON_BOOT_DISPLAY;
    }
    if fingerprint.topology_flags & TOPOLOGY_HOTPLUG_PORT != 0 {
        reasons |= REASON_NATIVE_GATED;
    }
    if fingerprint.topology_flags & TOPOLOGY_FIRMWARE_FRAMEBUFFER != 0 {
        reasons |= REASON_FIRMWARE_PRESERVED;
    }

    reasons
}

fn confidence_from_margin(margin: i64, strategy: DriverStrategy) -> u16 {
    if margin == i64::MAX {
        return u16::MAX;
    }
    if margin <= 0 {
        return 0;
    }

    let minimum = i64::from(MINIMUM_MARGIN[strategy.index()]);
    let scaled = margin
        .saturating_mul(i64::from(u16::MAX))
        .checked_div(minimum.max(1).saturating_mul(8))
        .unwrap_or(i64::from(u16::MAX));

    scaled.clamp(0, i64::from(u16::MAX)) as u16
}

fn set_feature(
    values: &mut [i16; FEATURE_COUNT],
    known_mask: &mut u32,
    index: usize,
    value: i16,
    known: bool,
) {
    values[index] = value;
    if known {
        *known_mask |= 1_u32 << index;
    }
}

fn normalize(values: &mut [i16; FEATURE_COUNT]) {
    for index in 0..FEATURE_COUNT {
        let centered = i32::from(values[index]) - i32::from(FEATURE_CENTER[index]);
        let scale = i32::from(FEATURE_SCALE[index]).max(1);
        let normalized = centered
            .saturating_mul(FEATURE_Q)
            .checked_div(scale)
            .unwrap_or(0);
        values[index] = normalized.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    }
}

fn vendor_bucket(vendor: u16) -> i16 {
    match vendor {
        VENDOR_NVIDIA => 256,
        VENDOR_AMD => 224,
        VENDOR_INTEL => 192,
        VENDOR_VIRTIO => 160,
        VENDOR_VMWARE => 144,
        VENDOR_REDHAT => 128,
        VENDOR_BOCHS => 112,
        0xffff => 0,
        _ => 64,
    }
}

fn bucket_u16(value: u16) -> i16 {
    ((u32::from(value) * 255) / u32::from(u16::MAX)) as i16
}

fn capability_density(flags: u32) -> i16 {
    (flags.count_ones().min(15) * 16) as i16
}

fn bool_feature(value: bool) -> i16 {
    if value { 256 } else { 0 }
}

fn bar_count(fingerprint: &GpuFingerprint) -> i16 {
    (fingerprint
        .bars
        .iter()
        .filter(|bar| bar.present())
        .count()
        .min(6)
        * 42) as i16
}

fn mmio64_count(fingerprint: &GpuFingerprint) -> i16 {
    (fingerprint
        .bars
        .iter()
        .filter(|bar| bar.flags & super::fingerprint::BAR_64BIT != 0)
        .count()
        .min(6)
        * 42) as i16
}

fn prefetchable_count(fingerprint: &GpuFingerprint) -> i16 {
    (fingerprint
        .bars
        .iter()
        .filter(|bar| bar.flags & super::fingerprint::BAR_PREFETCHABLE != 0)
        .count()
        .min(6)
        * 42) as i16
}

fn resource_tier(bytes: u64) -> i16 {
    let mib = bytes / (1024 * 1024);
    match mib {
        0 => 0,
        1..=15 => 32,
        16..=63 => 64,
        64..=255 => 96,
        256..=1023 => 128,
        1024..=4095 => 160,
        4096..=16383 => 192,
        _ => 224,
    }
}

fn feature_root(fingerprint_root: u64, values: &[i16; FEATURE_COUNT], known_mask: u32) -> u64 {
    let mut state = mix(fingerprint_root, u64::from(known_mask));
    for value in values {
        state = mix(state, u64::from(*value as u16));
    }
    state
}

fn decision_root(secret: u64, decision: &OracleDecision) -> u64 {
    let mut state = mix(secret, decision.fingerprint_root);
    state = mix(state, decision.feature_root);
    state = mix(state, u64::from(decision.model_schema));
    state = mix(state, decision.model_corpus_root);
    state = mix(state, decision.model_root);
    state = mix(state, decision.best_margin as u64);
    state = mix(state, u64::from(decision.confidence_q16));
    state = mix(state, u64::from(decision.reasons));
    state = mix(state, decision.length as u64);

    for candidate in decision.candidates() {
        state = mix(state, candidate.strategy.index() as u64);
        state = mix(state, candidate.score as u64);
        state = mix(state, candidate.margin as u64);
        state = mix(state, candidate.admissible as u64);
        state = mix(state, u64::from(candidate.gate_reason));
    }

    state
}

fn mix(mut state: u64, word: u64) -> u64 {
    state ^= word.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^ (state >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::drivernet::fingerprint::{
        BAR_64BIT, BAR_PRESENT, BarEvidence, FirmwareFramebufferEvidence, FirmwareFramebufferKind,
        PCI_CLASS_DISPLAY, PCI_SUBCLASS_VGA, TOPOLOGY_IOMMU_ISOLATED,
    };

    fn fingerprint(vendor: u16) -> GpuFingerprint {
        let mut fingerprint = GpuFingerprint::EMPTY;
        fingerprint.vendor_id = vendor;
        fingerprint.device_id = 1;
        fingerprint.class_code = 0x03;
        fingerprint.subclass = PCI_SUBCLASS_VGA;
        fingerprint.topology_flags = TOPOLOGY_IOMMU_ISOLATED;
        fingerprint.bars[0] = BarEvidence {
            raw_low: 0x1000_0004,
            raw_high: 0,
            length: 16 * 1024 * 1024,
            flags: BAR_PRESENT | BAR_64BIT,
        };
        fingerprint.evidence_root = 1;
        fingerprint
    }

    #[test]
    fn hard_gate_prevents_cross_vendor_native_dispatch() {
        let oracle = CompatibilityOracle::new(7, OraclePolicy::BLACK_LAB).unwrap();
        let decision = oracle.classify(&fingerprint(VENDOR_AMD)).unwrap();

        let hermes = decision
            .candidates()
            .iter()
            .find(|candidate| candidate.strategy == DriverStrategy::HermesNvidia)
            .unwrap();
        assert!(!hermes.admissible);
    }

    #[test]
    fn low_evidence_promotes_firmware_framebuffer() {
        let oracle = CompatibilityOracle::new(7, OraclePolicy::BLACK_LAB).unwrap();
        let mut fingerprint = GpuFingerprint::EMPTY;
        fingerprint.class_code = PCI_CLASS_DISPLAY;
        fingerprint.firmware_framebuffer = FirmwareFramebufferEvidence {
            kind: FirmwareFramebufferKind::UefiGop,
            width: 1920,
            height: 1080,
            pitch: 7680,
            format: 1,
            byte_length: 8_294_400,
            owner: None,
            retained: true,
        };
        fingerprint.evidence_root = 1;

        let decision = oracle.classify(&fingerprint).unwrap();
        assert_eq!(
            decision.candidates()[0].strategy,
            DriverStrategy::FirmwareFramebuffer
        );
    }
}
