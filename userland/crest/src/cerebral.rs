use slope::SyscallError;
use slope::process::tachyon;

use crate::input::InputEvent;

pub trait InputSource {
    type Error;

    fn try_read(&mut self) -> Result<Option<InputEvent>, Self::Error>;
}

pub trait CadencePredictor {
    fn predict_next(&mut self, observed: InputEvent) -> Option<InputEvent>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputPrediction {
    pub observed: InputEvent,
    /// Advisory only. It must never be injected as trusted user input.
    pub predicted: Option<InputEvent>,
}

pub struct NervousSystem<Source, Predictor> {
    source: Source,
    predictor: Predictor,
}

impl<Source: InputSource, Predictor: CadencePredictor> NervousSystem<Source, Predictor> {
    pub const fn new(source: Source, predictor: Predictor) -> Self {
        Self { source, predictor }
    }

    /// Performs one bounded observation. The kernel HID driver owns register
    /// acknowledgment and interrupt handling.
    pub fn observe_and_predict(&mut self) -> Result<Observation, CerebralError<Source::Error>> {
        match self.source.try_read().map_err(CerebralError::Input)? {
            Some(observed) => Ok(Observation::Event(InputPrediction {
                observed,
                predicted: self.predictor.predict_next(observed),
            })),
            None => Ok(Observation::Idle {
                yield_error: tachyon::yield_retrocausally(10).err(),
            }),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum Observation {
    Event(InputPrediction),
    Idle { yield_error: Option<SyscallError> },
}

#[derive(Debug, Eq, PartialEq)]
pub enum CerebralError<InputError> {
    Input(InputError),
    Yield(SyscallError),
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Source;
    impl InputSource for Source {
        type Error = ();

        fn try_read(&mut self) -> Result<Option<InputEvent>, Self::Error> {
            Ok(Some(InputEvent::Key {
                code: 30,
                pressed: true,
            }))
        }
    }

    struct Predictor;
    impl CadencePredictor for Predictor {
        fn predict_next(&mut self, _observed: InputEvent) -> Option<InputEvent> {
            Some(InputEvent::Key {
                code: 31,
                pressed: true,
            })
        }
    }

    #[test]
    fn prediction_remains_separate_from_observed_input() {
        let mut nervous_system = NervousSystem::new(Source, Predictor);
        let Observation::Event(prediction) = nervous_system.observe_and_predict().unwrap() else {
            panic!("expected one input event");
        };
        assert_ne!(prediction.predicted, Some(prediction.observed));
    }
}
