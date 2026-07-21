use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

pub const MAXIMUM_MULTIPLY_ACCUMULATES: usize = 1_000_000;
pub const CLASSIFIER_FEATURES: usize = 8;
const FIXED_POINT_ONE: i64 = 1 << 16;
const SNAPSHOT_ATTEMPTS: usize = 3;

/// Fixed-shape, heapless two-layer INT8 network.
///
/// Placing a zero-initialized instance in a `static` permits `.bss` storage;
/// nonzero static weights normally occupy `.data`. The type itself does not
/// dictate linker placement. Callers should defer inference from interrupt
/// context unless the selected dimensions have a verified execution bound.
pub struct QuantizedNetwork<const INPUTS: usize, const HIDDEN: usize, const OUTPUTS: usize> {
    hidden_weights: [[i8; INPUTS]; HIDDEN],
    hidden_biases: [i32; HIDDEN],
    output_weights: [[i8; HIDDEN]; OUTPUTS],
    output_biases: [i32; OUTPUTS],
    shift: u8,
}

impl<const INPUTS: usize, const HIDDEN: usize, const OUTPUTS: usize>
    QuantizedNetwork<INPUTS, HIDDEN, OUTPUTS>
{
    pub const fn new(
        hidden_weights: [[i8; INPUTS]; HIDDEN],
        hidden_biases: [i32; HIDDEN],
        output_weights: [[i8; HIDDEN]; OUTPUTS],
        output_biases: [i32; OUTPUTS],
        shift: u8,
    ) -> Result<Self, NetworkError> {
        if shift > 31 {
            return Err(NetworkError::InvalidShift);
        }
        let Some(hidden_operations) = INPUTS.checked_mul(HIDDEN) else {
            return Err(NetworkError::OperationLimitExceeded);
        };
        let Some(output_operations) = HIDDEN.checked_mul(OUTPUTS) else {
            return Err(NetworkError::OperationLimitExceeded);
        };
        let Some(operations) = hidden_operations.checked_add(output_operations) else {
            return Err(NetworkError::OperationLimitExceeded);
        };
        if operations > MAXIMUM_MULTIPLY_ACCUMULATES {
            return Err(NetworkError::OperationLimitExceeded);
        }
        Ok(Self {
            hidden_weights,
            hidden_biases,
            output_weights,
            output_biases,
            shift,
        })
    }

    pub fn infer(&self, inputs: &[i8; INPUTS]) -> [i32; OUTPUTS] {
        let mut hidden = [0_i32; HIDDEN];
        for (neuron, activation) in hidden.iter_mut().enumerate() {
            let mut accumulator = i64::from(self.hidden_biases[neuron]);
            for (input, weight) in inputs.iter().zip(self.hidden_weights[neuron].iter()) {
                accumulator += i64::from(*input) * i64::from(*weight);
            }
            *activation = clamp_i64_to_i32((accumulator >> self.shift).max(0));
        }

        let mut outputs = [0_i32; OUTPUTS];
        for (neuron, output) in outputs.iter_mut().enumerate() {
            let mut accumulator = i64::from(self.output_biases[neuron]);
            for (activation, weight) in hidden.iter().zip(self.output_weights[neuron].iter()) {
                accumulator += i64::from(*activation) * i64::from(*weight);
            }
            *output = clamp_i64_to_i32(accumulator >> self.shift);
        }
        outputs
    }

    pub const fn multiply_accumulates(&self) -> usize {
        INPUTS * HIDDEN + HIDDEN * OUTPUTS
    }
}

const fn clamp_i64_to_i32(value: i64) -> i32 {
    if value > i32::MAX as i64 {
        i32::MAX
    } else if value < i32::MIN as i64 {
        i32::MIN
    } else {
        value as i32
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkError {
    InvalidShift,
    OperationLimitExceeded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i8)]
pub enum Label {
    Benign = -1,
    Suspicious = 1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Classification {
    pub score_q16: i64,
    pub label: Label,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LearningOutcome {
    pub changed: bool,
    pub hinge_loss_q16: i64,
    pub step_q16: i64,
}

/// Online PA-I linear classifier with atomically published Q16.16 weights.
///
/// Updates are single-writer and classifications use a bounded sequence
/// snapshot. Callers must defer learning from interrupt context. Scores are
/// advisory telemetry and must never authorize or deny memory access.
pub struct PassiveAggressiveClassifier {
    weights_q16: [AtomicI64; CLASSIFIER_FEATURES],
    aggressiveness_q16: i64,
    sequence: AtomicU64,
    writer: AtomicBool,
}

impl PassiveAggressiveClassifier {
    pub const fn new() -> Self {
        Self {
            weights_q16: [const { AtomicI64::new(0) }; CLASSIFIER_FEATURES],
            aggressiveness_q16: 2 * FIXED_POINT_ONE,
            sequence: AtomicU64::new(0),
            writer: AtomicBool::new(false),
        }
    }

    pub const fn with_aggressiveness_q16(aggressiveness_q16: i64) -> Option<Self> {
        if aggressiveness_q16 <= 0 {
            return None;
        }
        Some(Self {
            weights_q16: [const { AtomicI64::new(0) }; CLASSIFIER_FEATURES],
            aggressiveness_q16,
            sequence: AtomicU64::new(0),
            writer: AtomicBool::new(false),
        })
    }

    pub fn classify(
        &self,
        features: &[i32; CLASSIFIER_FEATURES],
    ) -> Result<Classification, ClassifierError> {
        for _ in 0..SNAPSHOT_ATTEMPTS {
            let before = self.sequence.load(Ordering::Acquire);
            if before & 1 != 0 {
                continue;
            }
            let score_q16 = self.dot_product_q16(features);
            let after = self.sequence.load(Ordering::Acquire);
            if before == after {
                return Ok(Classification {
                    score_q16,
                    label: if score_q16 >= 0 {
                        Label::Suspicious
                    } else {
                        Label::Benign
                    },
                });
            }
        }
        Err(ClassifierError::UnstableSnapshot)
    }

    pub fn learn(
        &self,
        features: &[i32; CLASSIFIER_FEATURES],
        expected: Label,
    ) -> Result<LearningOutcome, ClassifierError> {
        self.writer
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| ClassifierError::UpdateInProgress)?;

        let score_q16 = self.dot_product_q16(features);
        let signed_margin = i128::from(score_q16) * i128::from(expected as i8);
        let loss = (i128::from(FIXED_POINT_ONE) - signed_margin).max(0);
        if loss == 0 {
            self.writer.store(false, Ordering::Release);
            return Ok(LearningOutcome {
                changed: false,
                hinge_loss_q16: 0,
                step_q16: 0,
            });
        }

        let norm_squared = features.iter().fold(0_i128, |norm, feature| {
            norm + i128::from(*feature) * i128::from(*feature)
        });
        if norm_squared == 0 {
            self.writer.store(false, Ordering::Release);
            return Err(ClassifierError::ZeroFeatureNorm);
        }
        let step = (loss / norm_squared).min(i128::from(self.aggressiveness_q16));

        self.sequence.fetch_add(1, Ordering::AcqRel);
        for (weight, feature) in self.weights_q16.iter().zip(features.iter()) {
            let old = i128::from(weight.load(Ordering::Relaxed));
            let delta = step * i128::from(expected as i8) * i128::from(*feature);
            weight.store(clamp_i128_to_i64(old + delta), Ordering::Relaxed);
        }
        self.sequence.fetch_add(1, Ordering::Release);
        self.writer.store(false, Ordering::Release);

        Ok(LearningOutcome {
            changed: true,
            hinge_loss_q16: clamp_i128_to_i64(loss),
            step_q16: clamp_i128_to_i64(step),
        })
    }

    fn dot_product_q16(&self, features: &[i32; CLASSIFIER_FEATURES]) -> i64 {
        let score =
            self.weights_q16
                .iter()
                .zip(features.iter())
                .fold(0_i128, |sum, (weight, feature)| {
                    sum + i128::from(weight.load(Ordering::Relaxed)) * i128::from(*feature)
                });
        clamp_i128_to_i64(score)
    }
}

impl Default for PassiveAggressiveClassifier {
    fn default() -> Self {
        Self::new()
    }
}

const fn clamp_i128_to_i64(value: i128) -> i64 {
    if value > i64::MAX as i128 {
        i64::MAX
    } else if value < i64::MIN as i128 {
        i64::MIN
    } else {
        value as i64
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClassifierError {
    UpdateInProgress,
    UnstableSnapshot,
    ZeroFeatureNorm,
}

pub static NYX_ANOMALY_DETECTOR: PassiveAggressiveClassifier = PassiveAggressiveClassifier::new();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn performs_a_deterministic_integer_forward_pass() {
        let network =
            QuantizedNetwork::<2, 2, 1>::new([[2, -1], [1, 1]], [0, 0], [[3, -2]], [0], 1).unwrap();
        assert_eq!(network.infer(&[4, 2]), [1]);
        assert_eq!(network.multiply_accumulates(), 6);
    }

    #[test]
    fn rejects_invalid_quantization_configuration() {
        assert!(matches!(
            QuantizedNetwork::<1, 1, 1>::new([[0]], [0], [[0]], [0], 32),
            Err(NetworkError::InvalidShift)
        ));
    }

    #[test]
    fn clamps_large_outputs_instead_of_wrapping() {
        let network =
            QuantizedNetwork::<1, 1, 1>::new([[127]], [i32::MAX], [[127]], [i32::MAX], 0).unwrap();
        assert_eq!(network.infer(&[127]), [i32::MAX]);
    }

    #[test]
    fn passive_aggressive_updates_follow_hinge_loss_and_feature_norm() {
        let classifier = PassiveAggressiveClassifier::new();
        let features = [1, 0, 0, 0, 0, 0, 0, 0];
        let first = classifier.learn(&features, Label::Suspicious).unwrap();
        assert!(first.changed);
        assert_eq!(
            classifier.classify(&features).unwrap().label,
            Label::Suspicious
        );
        let correction = classifier.learn(&features, Label::Benign).unwrap();
        assert!(correction.changed);
        assert_eq!(classifier.classify(&features).unwrap().label, Label::Benign);
    }

    #[test]
    fn passive_aggressive_rejects_an_unlearnable_zero_vector() {
        let classifier = PassiveAggressiveClassifier::new();
        assert_eq!(
            classifier.learn(&[0; CLASSIFIER_FEATURES], Label::Benign),
            Err(ClassifierError::ZeroFeatureNorm)
        );
    }
}
