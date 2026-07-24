use crate::pythia::QuantizedNetwork;

pub const THERMAL_INPUTS: usize = 4;
pub const THERMAL_HIDDEN: usize = 8;
pub const THERMAL_OUTPUTS: usize = 1;

pub type ThermalNetwork = QuantizedNetwork<THERMAL_INPUTS, THERMAL_HIDDEN, THERMAL_OUTPUTS>;

const fn sample_network() -> ThermalNetwork {
    match ThermalNetwork::new(
        [[1; THERMAL_INPUTS]; THERMAL_HIDDEN],
        [0; THERMAL_HIDDEN],
        [[1; THERMAL_HIDDEN]; THERMAL_OUTPUTS],
        [0; THERMAL_OUTPUTS],
        4,
    ) {
        Ok(network) => network,
        Err(_) => panic!("invalid built-in thermal model"),
    }
}

/// Uncertified diagnostic network used to exercise the inference path.
///
/// It is deliberately excluded from power-management decisions: `advise`
/// requires an accepted validation report before it can emit an action.
pub static DIAGNOSTIC_THERMAL_NETWORK: ThermalNetwork = sample_network();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThermalSample {
    pub runnable_threads: u32,
    pub semantic_heat: u64,
    pub flux_rate: u64,
    pub current_temperature_millicelsius: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputQuantization {
    pub runnable_threads_per_unit: u32,
    pub semantic_heat_per_unit: u64,
    pub flux_per_unit: u64,
    pub millicelsius_per_unit: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValidationReport {
    pub horizon_milliseconds: u32,
    pub validation_samples: u32,
    pub maximum_error_millicelsius: u32,
}

impl ValidationReport {
    pub const MINIMUM_SAMPLES: u32 = 1_000;
    pub const MAXIMUM_ACCEPTABLE_ERROR_MILLICELSIUS: u32 = 5_000;

    const fn is_acceptable(self) -> bool {
        self.horizon_milliseconds == 500
            && self.validation_samples >= Self::MINIMUM_SAMPLES
            && self.maximum_error_millicelsius <= Self::MAXIMUM_ACCEPTABLE_ERROR_MILLICELSIUS
    }
}

pub struct ThermalOracle {
    network: &'static ThermalNetwork,
    input: InputQuantization,
    output_millicelsius_per_unit: i32,
    output_offset_millicelsius: i32,
    validation: Option<ValidationReport>,
}

impl ThermalOracle {
    pub const fn new(
        network: &'static ThermalNetwork,
        input: InputQuantization,
        output_millicelsius_per_unit: i32,
        output_offset_millicelsius: i32,
        validation: Option<ValidationReport>,
    ) -> Result<Self, ThermalError> {
        if input.runnable_threads_per_unit == 0
            || input.semantic_heat_per_unit == 0
            || input.flux_per_unit == 0
            || input.millicelsius_per_unit == 0
            || output_millicelsius_per_unit == 0
        {
            return Err(ThermalError::InvalidQuantization);
        }
        if let Some(report) = validation {
            if !report.is_acceptable() {
                return Err(ThermalError::InvalidValidation);
            }
        }
        Ok(Self {
            network,
            input,
            output_millicelsius_per_unit,
            output_offset_millicelsius,
            validation,
        })
    }

    pub fn forecast(&self, sample: ThermalSample) -> ThermalForecast {
        let inputs = [
            quantize_unsigned(
                u64::from(sample.runnable_threads),
                u64::from(self.input.runnable_threads_per_unit),
            ),
            quantize_unsigned(sample.semantic_heat, self.input.semantic_heat_per_unit),
            quantize_unsigned(sample.flux_rate, self.input.flux_per_unit),
            quantize_signed(
                sample.current_temperature_millicelsius,
                self.input.millicelsius_per_unit,
            ),
        ];
        let units = i64::from(self.network.infer(&inputs)[0]);
        let temperature = units
            .saturating_mul(i64::from(self.output_millicelsius_per_unit))
            .saturating_add(i64::from(self.output_offset_millicelsius));
        ThermalForecast {
            horizon_milliseconds: self
                .validation
                .map_or(500, |report| report.horizon_milliseconds),
            predicted_temperature_millicelsius: clamp_i64_to_i32(temperature),
            validated: self.validation.is_some(),
        }
    }

    pub fn advise(
        &self,
        sample: ThermalSample,
        brake_threshold_millicelsius: i32,
    ) -> Result<ThermalAdvice, ThermalError> {
        if self.validation.is_none() {
            return Err(ThermalError::UnvalidatedModel);
        }
        let forecast = self.forecast(sample);
        if forecast.predicted_temperature_millicelsius >= brake_threshold_millicelsius {
            Ok(ThermalAdvice::ReducePowerCeiling { forecast })
        } else {
            Ok(ThermalAdvice::Hold { forecast })
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThermalForecast {
    pub horizon_milliseconds: u32,
    pub predicted_temperature_millicelsius: i32,
    pub validated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThermalAdvice {
    Hold { forecast: ThermalForecast },
    ReducePowerCeiling { forecast: ThermalForecast },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThermalError {
    InvalidQuantization,
    InvalidValidation,
    UnvalidatedModel,
}

fn quantize_unsigned(value: u64, units: u64) -> i8 {
    (value / units).min(i8::MAX as u64) as i8
}

fn quantize_signed(value: i32, units: u32) -> i8 {
    let quantized = i64::from(value) / i64::from(units);
    quantized.clamp(i64::from(i8::MIN), i64::from(i8::MAX)) as i8
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

#[cfg(test)]
mod tests {
    use super::*;

    static CURRENT_TEMPERATURE_MODEL: ThermalNetwork = match ThermalNetwork::new(
        [
            [0, 0, 0, 1],
            [0; 4],
            [0; 4],
            [0; 4],
            [0; 4],
            [0; 4],
            [0; 4],
            [0; 4],
        ],
        [0; 8],
        [[1, 0, 0, 0, 0, 0, 0, 0]],
        [20],
        0,
    ) {
        Ok(network) => network,
        Err(_) => panic!("invalid test model"),
    };

    const QUANTIZATION: InputQuantization = InputQuantization {
        runnable_threads_per_unit: 1,
        semantic_heat_per_unit: 100,
        flux_per_unit: 100,
        millicelsius_per_unit: 1_000,
    };

    const VALIDATION: ValidationReport = ValidationReport {
        horizon_milliseconds: 500,
        validation_samples: 2_000,
        maximum_error_millicelsius: 2_000,
    };

    #[test]
    fn validated_forecast_can_emit_a_brake_advisory() {
        let oracle = ThermalOracle::new(
            &CURRENT_TEMPERATURE_MODEL,
            QUANTIZATION,
            1_000,
            0,
            Some(VALIDATION),
        )
        .unwrap();
        let advice = oracle
            .advise(
                ThermalSample {
                    runnable_threads: 4,
                    semantic_heat: 10_000,
                    flux_rate: 5_000,
                    current_temperature_millicelsius: 70_000,
                },
                85_000,
            )
            .unwrap();
        assert!(matches!(advice, ThermalAdvice::ReducePowerCeiling { .. }));
    }

    #[test]
    fn uncertified_model_cannot_drive_power_management() {
        let oracle =
            ThermalOracle::new(&DIAGNOSTIC_THERMAL_NETWORK, QUANTIZATION, 1_000, 0, None).unwrap();
        assert_eq!(
            oracle.advise(
                ThermalSample {
                    runnable_threads: 127,
                    semantic_heat: u64::MAX,
                    flux_rate: u64::MAX,
                    current_temperature_millicelsius: 100_000,
                },
                85_000,
            ),
            Err(ThermalError::UnvalidatedModel)
        );
    }
}
