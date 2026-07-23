pub const REGISTER_COUNT: usize = 16;
pub const MAX_INSTRUCTIONS: usize = 64;
pub const MAXIMUM_FUEL: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Opcode {
    Nop = 0,
    LoadMetric = 1,
    LoadImmediate = 2,
    AddSaturating = 3,
    SubtractSaturating = 4,
    MultiplyQ16 = 5,
    CompareGreater = 6,
    JumpIfZero = 7,
    SetControl = 8,
    Halt = 9,
}

impl Opcode {
    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Nop),
            1 => Some(Self::LoadMetric),
            2 => Some(Self::LoadImmediate),
            3 => Some(Self::AddSaturating),
            4 => Some(Self::SubtractSaturating),
            5 => Some(Self::MultiplyQ16),
            6 => Some(Self::CompareGreater),
            7 => Some(Self::JumpIfZero),
            8 => Some(Self::SetControl),
            9 => Some(Self::Halt),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum MetricId {
    Heat = 0,
    QueuePressure = 1,
    CollapseRate = 2,
    PhaseDrift = 3,
    ReplayPressure = 4,
    Coherence = 5,
    KernelPhase = 6,
}

impl MetricId {
    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Heat),
            1 => Some(Self::QueuePressure),
            2 => Some(Self::CollapseRate),
            3 => Some(Self::PhaseDrift),
            4 => Some(Self::ReplayPressure),
            5 => Some(Self::Coherence),
            6 => Some(Self::KernelPhase),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ControlField {
    PriorityMass = 0,
    CollapseThreshold = 1,
    TargetPhase = 2,
    QuarantineTicks = 3,
    Flags = 4,
    HeatCeiling = 5,
}

impl ControlField {
    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::PriorityMass),
            1 => Some(Self::CollapseThreshold),
            2 => Some(Self::TargetPhase),
            3 => Some(Self::QuarantineTicks),
            4 => Some(Self::Flags),
            5 => Some(Self::HeatCeiling),
            _ => None,
        }
    }

    const fn mask(self) -> u32 {
        1 << self as u8
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct LabInstruction(u64);

impl LabInstruction {
    pub const ZERO: Self = Self(0);

    pub const fn new(
        opcode: Opcode,
        destination: u8,
        source: u8,
        immediate: i32,
        auxiliary: u8,
    ) -> Self {
        Self(
            opcode as u64
                | (((destination & 0x0f) as u64) << 8)
                | (((source & 0x0f) as u64) << 12)
                | ((immediate as u32 as u64) << 16)
                | ((auxiliary as u64) << 48),
        )
    }

    fn opcode(self) -> Option<Opcode> {
        Opcode::from_raw(self.0 as u8)
    }

    const fn destination(self) -> usize {
        ((self.0 >> 8) & 0x0f) as usize
    }

    const fn source(self) -> usize {
        ((self.0 >> 12) & 0x0f) as usize
    }

    const fn immediate(self) -> i32 {
        ((self.0 >> 16) as u32) as i32
    }

    const fn auxiliary(self) -> u8 {
        (self.0 >> 48) as u8
    }
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct LabProgram {
    pub instructions: [LabInstruction; MAX_INSTRUCTIONS],
    pub length: u8,
    pub reserved: [u8; 7],
}

impl LabProgram {
    pub const fn new(instructions: [LabInstruction; MAX_INSTRUCTIONS], length: u8) -> Self {
        Self {
            instructions,
            length,
            reserved: [0; 7],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LabMetrics {
    pub heat: i64,
    pub queue_pressure: i64,
    pub collapse_rate: i64,
    pub phase_drift: i64,
    pub replay_pressure: i64,
    pub coherence: i64,
    pub kernel_phase: i64,
}

impl LabMetrics {
    fn read(self, metric: MetricId) -> i64 {
        match metric {
            MetricId::Heat => self.heat,
            MetricId::QueuePressure => self.queue_pressure,
            MetricId::CollapseRate => self.collapse_rate,
            MetricId::PhaseDrift => self.phase_drift,
            MetricId::ReplayPressure => self.replay_pressure,
            MetricId::Coherence => self.coherence,
            MetricId::KernelPhase => self.kernel_phase,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlVector {
    pub mask: u32,

    pub priority_mass: i64,
    pub collapse_threshold: i64,
    pub target_phase: i64,
    pub quarantine_ticks: i64,
    pub flags: i64,
    pub heat_ceiling: i64,
}

impl ControlVector {
    pub const ZERO: Self = Self {
        mask: 0,
        priority_mass: 0,
        collapse_threshold: 0,
        target_phase: 0,
        quarantine_ticks: 0,
        flags: 0,
        heat_ceiling: 0,
    };

    fn set(&mut self, field: ControlField, value: i64) {
        self.mask |= field.mask();

        match field {
            ControlField::PriorityMass => {
                self.priority_mass = value;
            }
            ControlField::CollapseThreshold => {
                self.collapse_threshold = value;
            }
            ControlField::TargetPhase => {
                self.target_phase = value;
            }
            ControlField::QuarantineTicks => {
                self.quarantine_ticks = value;
            }
            ControlField::Flags => {
                self.flags = value;
            }
            ControlField::HeatCeiling => {
                self.heat_ceiling = value;
            }
        }
    }

    pub const fn contains(&self, field: ControlField) -> bool {
        self.mask & field.mask() != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerifyError {
    Empty,
    TooLong,
    BadOpcode(usize),
    BadMetric(usize),
    BadControl(usize),
    InvalidBranch(usize),
    MissingHalt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionError {
    Verification(VerifyError),
    FuelExhausted,
    FellOffProgram,
}

pub fn verify(program: &LabProgram) -> Result<(), VerifyError> {
    let length = usize::from(program.length);

    if length == 0 {
        return Err(VerifyError::Empty);
    }

    if length > MAX_INSTRUCTIONS {
        return Err(VerifyError::TooLong);
    }

    let mut has_halt = false;

    for pc in 0..length {
        let instruction = program.instructions[pc];

        let opcode = instruction.opcode().ok_or(VerifyError::BadOpcode(pc))?;

        match opcode {
            Opcode::LoadMetric => {
                MetricId::from_raw(instruction.immediate() as u8)
                    .ok_or(VerifyError::BadMetric(pc))?;
            }

            Opcode::SetControl => {
                ControlField::from_raw(instruction.auxiliary())
                    .ok_or(VerifyError::BadControl(pc))?;
            }

            Opcode::JumpIfZero => {
                let target = usize::from(instruction.auxiliary());

                if target <= pc || target >= length {
                    return Err(VerifyError::InvalidBranch(pc));
                }
            }

            Opcode::Halt => has_halt = true,

            _ => {}
        }
    }

    if !has_halt {
        return Err(VerifyError::MissingHalt);
    }

    Ok(())
}

pub fn execute(program: &LabProgram, metrics: LabMetrics) -> Result<ControlVector, ExecutionError> {
    verify(program).map_err(ExecutionError::Verification)?;

    let length = usize::from(program.length);
    let mut registers = [0_i64; REGISTER_COUNT];
    let mut control = ControlVector::ZERO;

    let mut pc = 0_usize;
    let mut fuel = MAXIMUM_FUEL;

    while pc < length {
        if fuel == 0 {
            return Err(ExecutionError::FuelExhausted);
        }

        fuel -= 1;

        let instruction = program.instructions[pc];
        let opcode = instruction.opcode().ok_or(ExecutionError::FellOffProgram)?;

        let destination = instruction.destination();
        let source = instruction.source();

        match opcode {
            Opcode::Nop => {}

            Opcode::LoadMetric => {
                let metric = MetricId::from_raw(instruction.immediate() as u8)
                    .ok_or(ExecutionError::FellOffProgram)?;

                registers[destination] = metrics.read(metric);
            }

            Opcode::LoadImmediate => {
                registers[destination] = i64::from(instruction.immediate());
            }

            Opcode::AddSaturating => {
                registers[destination] = registers[destination].saturating_add(registers[source]);
            }

            Opcode::SubtractSaturating => {
                registers[destination] = registers[destination].saturating_sub(registers[source]);
            }

            Opcode::MultiplyQ16 => {
                let product = i128::from(registers[destination]) * i128::from(registers[source]);

                registers[destination] =
                    (product >> 16).clamp(i64::MIN as i128, i64::MAX as i128) as i64;
            }

            Opcode::CompareGreater => {
                registers[destination] = i64::from(registers[destination] > registers[source]);
            }

            Opcode::JumpIfZero => {
                if registers[destination] == 0 {
                    pc = usize::from(instruction.auxiliary());
                    continue;
                }
            }

            Opcode::SetControl => {
                let field = ControlField::from_raw(instruction.auxiliary())
                    .ok_or(ExecutionError::FellOffProgram)?;

                control.set(field, registers[destination]);
            }

            Opcode::Halt => return Ok(control),
        }

        pc += 1;
    }

    Err(ExecutionError::FellOffProgram)
}
