#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Effect {
    Yield,
    SleepUntil(u64),
    AllocatePages {
        count: u32,
        numa_domain: u16,
    },
    Map {
        virtual_address: u64,
        physical_address: u64,
        length: u64,
        provenance: u32,
    },
    SessionSend {
        endpoint: u64,
        payload: u64,
    },
    SessionReceive {
        endpoint: u64,
        maximum_length: u32,
    },
    Trace {
        kind: u16,
        argument_zero: u64,
        argument_one: u64,
    },
    Policy {
        program: u16,
        argument: u64,
    },
    Fabric {
        opcode: u32,
        source_handle: u64,
        destination_handle: u64,
        length: u64,
    },
    Custom {
        opcode: u32,
        argument_zero: u64,
        argument_one: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reply {
    pub status: i32,
    pub value: u64,
}

impl Reply {
    pub const fn success(value: u64) -> Self {
        Self { status: 0, value }
    }

    pub const fn failure(status: i32) -> Self {
        Self { status, value: 0 }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step<Output> {
    Complete(Output),
    Perform(Effect),
}

/// Explicit state machine for a computation that requests kernel effects.
///
/// State machines remain allocation-free and make every suspension point
/// visible in their implementation.
pub trait Effectful {
    type Output;

    fn step(&mut self, reply: Option<Reply>) -> Step<Self::Output>;
}

pub trait Handler {
    fn handle(&mut self, effect: Effect) -> Option<Reply>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunError {
    Unhandled(Effect),
}

pub fn run<Program, EffectHandler>(
    program: &mut Program,
    handler: &mut EffectHandler,
) -> Result<Program::Output, RunError>
where
    Program: Effectful,
    EffectHandler: Handler,
{
    let mut reply = None;
    loop {
        match program.step(reply.take()) {
            Step::Complete(output) => return Ok(output),
            Step::Perform(effect) => {
                reply = Some(handler.handle(effect).ok_or(RunError::Unhandled(effect))?);
            }
        }
    }
}

pub struct HandlerStack<First, Second> {
    pub first: First,
    pub second: Second,
}

impl<First: Handler, Second: Handler> Handler for HandlerStack<First, Second> {
    fn handle(&mut self, effect: Effect) -> Option<Reply> {
        self.first
            .handle(effect)
            .or_else(|| self.second.handle(effect))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Program {
        state: u8,
    }

    impl Effectful for Program {
        type Output = u64;

        fn step(&mut self, reply: Option<Reply>) -> Step<Self::Output> {
            match self.state {
                0 => {
                    self.state = 1;
                    Step::Perform(Effect::Policy {
                        program: 2,
                        argument: 41,
                    })
                }
                _ => Step::Complete(reply.unwrap().value + 1),
            }
        }
    }

    struct PolicyHandler;

    impl Handler for PolicyHandler {
        fn handle(&mut self, effect: Effect) -> Option<Reply> {
            match effect {
                Effect::Policy { argument, .. } => Some(Reply::success(argument)),
                _ => None,
            }
        }
    }

    #[test]
    fn runs_an_explicit_effect_state_machine() {
        let mut program = Program { state: 0 };
        assert_eq!(run(&mut program, &mut PolicyHandler), Ok(42));
    }

    #[test]
    fn fails_closed_for_an_unhandled_effect() {
        let mut program = Program { state: 0 };
        struct Empty;
        impl Handler for Empty {
            fn handle(&mut self, _effect: Effect) -> Option<Reply> {
                None
            }
        }
        assert_eq!(
            run(&mut program, &mut Empty),
            Err(RunError::Unhandled(Effect::Policy {
                program: 2,
                argument: 41,
            }))
        );
    }
}
