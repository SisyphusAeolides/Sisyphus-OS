use crate::sync::SpinLock;

pub const REGISTER_COUNT: usize = 16;
pub const MAXIMUM_PROGRAMS: usize = 16;
pub const MAXIMUM_INSTRUCTIONS: usize = 256;
pub const MAXIMUM_HOST_FUNCTIONS: usize = 32;
pub const DEFAULT_FUEL: u32 = 100_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Opcode {
    Nop = 0,
    MoveImmediate = 1,
    Move = 2,
    Add = 3,
    Subtract = 4,
    And = 5,
    Or = 6,
    Xor = 7,
    ShiftLeft = 8,
    ShiftRight = 9,
    SetLessOrEqual = 10,
    Jump = 11,
    JumpIfZero = 12,
    JumpIfNotZero = 13,
    CallHost = 14,
    Return = 15,
}

impl Opcode {
    fn decode(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::Nop,
            1 => Self::MoveImmediate,
            2 => Self::Move,
            3 => Self::Add,
            4 => Self::Subtract,
            5 => Self::And,
            6 => Self::Or,
            7 => Self::Xor,
            8 => Self::ShiftLeft,
            9 => Self::ShiftRight,
            10 => Self::SetLessOrEqual,
            11 => Self::Jump,
            12 => Self::JumpIfZero,
            13 => Self::JumpIfNotZero,
            14 => Self::CallHost,
            15 => Self::Return,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Instruction {
    pub opcode: u8,
    pub destination: u8,
    pub source: u8,
    pub second_source: u8,
    pub immediate: i32,
}

impl Instruction {
    pub const fn new(
        opcode: Opcode,
        destination: u8,
        source: u8,
        second_source: u8,
        immediate: i32,
    ) -> Self {
        Self {
            opcode: opcode as u8,
            destination,
            source,
            second_source,
            immediate,
        }
    }
}

const RETURN_ZERO: Instruction = Instruction::new(Opcode::Return, 0, 0, 0, 0);

#[derive(Clone, Copy)]
struct Program {
    code: [Instruction; MAXIMUM_INSTRUCTIONS],
    length: usize,
    entry: usize,
    generation: u32,
    live: bool,
}

impl Program {
    const fn empty() -> Self {
        Self {
            code: [RETURN_ZERO; MAXIMUM_INSTRUCTIONS],
            length: 0,
            entry: 0,
            generation: 0,
            live: false,
        }
    }
}

pub type HostFunction = fn(u64, u64, u64, u64) -> u64;

pub struct PolicyVm {
    programs: [SpinLock<Program>; MAXIMUM_PROGRAMS],
    hosts: SpinLock<[Option<HostFunction>; MAXIMUM_HOST_FUNCTIONS]>,
}

impl PolicyVm {
    pub const fn new() -> Self {
        Self {
            programs: [const { SpinLock::new(Program::empty()) }; MAXIMUM_PROGRAMS],
            hosts: SpinLock::new([None; MAXIMUM_HOST_FUNCTIONS]),
        }
    }

    pub fn register_host(&self, index: usize, function: HostFunction) -> Result<(), PolicyError> {
        let mut hosts = self.hosts.lock();
        let slot = hosts
            .get_mut(index)
            .ok_or(PolicyError::InvalidHostFunction)?;
        *slot = Some(function);
        Ok(())
    }

    /// Verifies and atomically publishes a policy program. Each run takes an
    /// immutable snapshot, so an in-flight run completes its original version.
    pub fn load(
        &self,
        slot: usize,
        instructions: &[Instruction],
        entry: usize,
    ) -> Result<u32, PolicyError> {
        verify(instructions, entry)?;
        let mut program = self
            .programs
            .get(slot)
            .ok_or(PolicyError::InvalidProgram)?
            .lock();
        program.live = false;
        program.code[..instructions.len()].copy_from_slice(instructions);
        program.length = instructions.len();
        program.entry = entry;
        program.generation = next_generation(program.generation);
        program.live = true;
        Ok(program.generation)
    }

    pub fn run(&self, slot: usize, arguments: [u64; 4]) -> Result<u64, PolicyError> {
        self.run_with_fuel(slot, arguments, DEFAULT_FUEL)
    }

    pub fn run_with_fuel(
        &self,
        slot: usize,
        arguments: [u64; 4],
        mut fuel: u32,
    ) -> Result<u64, PolicyError> {
        let program = *self
            .programs
            .get(slot)
            .ok_or(PolicyError::InvalidProgram)?
            .lock();
        if !program.live {
            return Err(PolicyError::ProgramUnavailable);
        }
        let mut registers = [0_u64; REGISTER_COUNT];
        registers[..4].copy_from_slice(&arguments);
        let mut pc = program.entry;

        while fuel != 0 {
            fuel -= 1;
            let instruction = *program
                .code
                .get(pc)
                .filter(|_| pc < program.length)
                .ok_or(PolicyError::InvalidProgramCounter)?;
            let opcode = Opcode::decode(instruction.opcode)
                .ok_or(PolicyError::InvalidInstruction { index: pc })?;
            let destination = usize::from(instruction.destination);
            let source = usize::from(instruction.source);
            let second_source = usize::from(instruction.second_source);

            match opcode {
                Opcode::Nop => pc += 1,
                Opcode::MoveImmediate => {
                    registers[destination] = instruction.immediate as i64 as u64;
                    pc += 1;
                }
                Opcode::Move => {
                    registers[destination] = registers[source];
                    pc += 1;
                }
                Opcode::Add => {
                    registers[destination] =
                        registers[source].wrapping_add(registers[second_source]);
                    pc += 1;
                }
                Opcode::Subtract => {
                    registers[destination] =
                        registers[source].wrapping_sub(registers[second_source]);
                    pc += 1;
                }
                Opcode::And => {
                    registers[destination] = registers[source] & registers[second_source];
                    pc += 1;
                }
                Opcode::Or => {
                    registers[destination] = registers[source] | registers[second_source];
                    pc += 1;
                }
                Opcode::Xor => {
                    registers[destination] = registers[source] ^ registers[second_source];
                    pc += 1;
                }
                Opcode::ShiftLeft => {
                    registers[destination] = registers[source] << (registers[second_source] & 63);
                    pc += 1;
                }
                Opcode::ShiftRight => {
                    registers[destination] = registers[source] >> (registers[second_source] & 63);
                    pc += 1;
                }
                Opcode::SetLessOrEqual => {
                    registers[destination] =
                        u64::from(registers[source] <= registers[second_source]);
                    pc += 1;
                }
                Opcode::Jump => pc = branch_target(pc, instruction.immediate)?,
                Opcode::JumpIfZero => {
                    if registers[source] == 0 {
                        pc = branch_target(pc, instruction.immediate)?;
                    } else {
                        pc += 1;
                    }
                }
                Opcode::JumpIfNotZero => {
                    if registers[source] != 0 {
                        pc = branch_target(pc, instruction.immediate)?;
                    } else {
                        pc += 1;
                    }
                }
                Opcode::CallHost => {
                    let host_index = usize::try_from(instruction.immediate)
                        .map_err(|_| PolicyError::InvalidHostFunction)?;
                    let function = self
                        .hosts
                        .lock()
                        .get(host_index)
                        .copied()
                        .flatten()
                        .ok_or(PolicyError::InvalidHostFunction)?;
                    registers[0] = function(registers[0], registers[1], registers[2], registers[3]);
                    pc += 1;
                }
                Opcode::Return => return Ok(registers[source]),
            }
        }
        Err(PolicyError::OutOfFuel)
    }
}

impl Default for PolicyVm {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyError {
    InvalidProgram,
    EmptyProgram,
    InvalidEntry,
    InvalidInstruction { index: usize },
    InvalidRegister { index: usize },
    InvalidBranch { index: usize },
    InvalidProgramCounter,
    InvalidHostFunction,
    ProgramUnavailable,
    OutOfFuel,
}

fn verify(instructions: &[Instruction], entry: usize) -> Result<(), PolicyError> {
    if instructions.is_empty() {
        return Err(PolicyError::EmptyProgram);
    }
    if instructions.len() > MAXIMUM_INSTRUCTIONS {
        return Err(PolicyError::InvalidProgram);
    }
    if entry >= instructions.len() {
        return Err(PolicyError::InvalidEntry);
    }
    let mut has_return = false;
    for (index, instruction) in instructions.iter().enumerate() {
        let opcode =
            Opcode::decode(instruction.opcode).ok_or(PolicyError::InvalidInstruction { index })?;
        verify_registers(index, *instruction, opcode)?;
        if matches!(
            opcode,
            Opcode::Jump | Opcode::JumpIfZero | Opcode::JumpIfNotZero
        ) {
            let target = branch_target(index, instruction.immediate)
                .map_err(|_| PolicyError::InvalidBranch { index })?;
            if target >= instructions.len() {
                return Err(PolicyError::InvalidBranch { index });
            }
        }
        if opcode == Opcode::CallHost
            && usize::try_from(instruction.immediate)
                .ok()
                .is_none_or(|host| host >= MAXIMUM_HOST_FUNCTIONS)
        {
            return Err(PolicyError::InvalidHostFunction);
        }
        has_return |= opcode == Opcode::Return;
    }
    if !has_return {
        return Err(PolicyError::InvalidProgram);
    }
    Ok(())
}

fn verify_registers(
    index: usize,
    instruction: Instruction,
    opcode: Opcode,
) -> Result<(), PolicyError> {
    let valid = |register: u8| usize::from(register) < REGISTER_COUNT;
    let registers = match opcode {
        Opcode::Nop | Opcode::Jump | Opcode::CallHost => [None, None, None],
        Opcode::MoveImmediate => [Some(instruction.destination), None, None],
        Opcode::Move => [
            Some(instruction.destination),
            Some(instruction.source),
            None,
        ],
        Opcode::JumpIfZero | Opcode::JumpIfNotZero | Opcode::Return => {
            [Some(instruction.source), None, None]
        }
        _ => [
            Some(instruction.destination),
            Some(instruction.source),
            Some(instruction.second_source),
        ],
    };
    if registers.into_iter().flatten().all(valid) {
        Ok(())
    } else {
        Err(PolicyError::InvalidRegister { index })
    }
}

fn branch_target(program_counter: usize, relative: i32) -> Result<usize, PolicyError> {
    let pc = i64::try_from(program_counter).map_err(|_| PolicyError::InvalidProgramCounter)?;
    usize::try_from(pc + i64::from(relative)).map_err(|_| PolicyError::InvalidProgramCounter)
}

const fn next_generation(generation: u32) -> u32 {
    let next = generation.wrapping_add(1);
    if next == 0 { 1 } else { next }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT_POLICY: [Instruction; 3] = [
        Instruction::new(Opcode::MoveImmediate, 1, 0, 0, 512),
        Instruction::new(Opcode::SetLessOrEqual, 0, 0, 1, 0),
        Instruction::new(Opcode::Return, 0, 0, 0, 0),
    ];

    #[test]
    fn verifies_and_runs_a_bounded_policy() {
        let vm = PolicyVm::new();
        assert_eq!(vm.load(0, &LIMIT_POLICY, 0), Ok(1));
        assert_eq!(vm.run(0, [128, 0, 0, 0]), Ok(1));
        assert_eq!(vm.run(0, [1024, 0, 0, 0]), Ok(0));
    }

    #[test]
    fn rejects_unknown_operations_registers_and_branches() {
        let vm = PolicyVm::new();
        let unknown = [Instruction {
            opcode: 0xff,
            ..RETURN_ZERO
        }];
        assert_eq!(
            vm.load(0, &unknown, 0),
            Err(PolicyError::InvalidInstruction { index: 0 })
        );
        let bad_register = [Instruction::new(Opcode::Return, 0, 20, 0, 0)];
        assert_eq!(
            vm.load(0, &bad_register, 0),
            Err(PolicyError::InvalidRegister { index: 0 })
        );
        let bad_branch = [Instruction::new(Opcode::Jump, 0, 0, 0, 4), RETURN_ZERO];
        assert_eq!(
            vm.load(0, &bad_branch, 0),
            Err(PolicyError::InvalidBranch { index: 0 })
        );
    }

    #[test]
    fn fuel_stops_a_verified_loop() {
        let vm = PolicyVm::new();
        let looping = [Instruction::new(Opcode::Jump, 0, 0, 0, 0), RETURN_ZERO];
        vm.load(0, &looping, 0).unwrap();
        assert_eq!(vm.run_with_fuel(0, [0; 4], 4), Err(PolicyError::OutOfFuel));
    }

    #[test]
    fn program_replacement_advances_generation() {
        let vm = PolicyVm::new();
        assert_eq!(vm.load(0, &LIMIT_POLICY, 0), Ok(1));
        assert_eq!(vm.load(0, &LIMIT_POLICY, 0), Ok(2));
    }
}
