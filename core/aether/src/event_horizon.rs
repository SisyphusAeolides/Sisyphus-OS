use crate::blacklab_vm::{
    ControlField, LabInstruction, LabProgram, MAX_INSTRUCTIONS, MetricId, Opcode,
};

pub const EVENT_HORIZON_PROGRAM: LabProgram = {
    let mut code = [LabInstruction::ZERO; MAX_INSTRUCTIONS];

    // if heat > 850_000:
    //     priority_mass = 0x3000
    code[0] = LabInstruction::new(Opcode::LoadMetric, 0, 0, MetricId::Heat as i32, 0);

    code[1] = LabInstruction::new(Opcode::LoadImmediate, 1, 0, 850_000, 0);

    code[2] = LabInstruction::new(Opcode::CompareGreater, 0, 1, 0, 0);

    code[3] = LabInstruction::new(Opcode::JumpIfZero, 0, 0, 0, 7);

    code[4] = LabInstruction::new(Opcode::LoadImmediate, 2, 0, 0x3000, 0);

    code[5] = LabInstruction::new(
        Opcode::SetControl,
        2,
        0,
        0,
        ControlField::PriorityMass as u8,
    );

    code[6] = LabInstruction::new(Opcode::Halt, 0, 0, 0, 0);

    // else if phase_drift > 192:
    //     target_phase = kernel_phase
    code[7] = LabInstruction::new(Opcode::LoadMetric, 3, 0, MetricId::PhaseDrift as i32, 0);

    code[8] = LabInstruction::new(Opcode::LoadImmediate, 4, 0, 192, 0);

    code[9] = LabInstruction::new(Opcode::CompareGreater, 3, 4, 0, 0);

    code[10] = LabInstruction::new(Opcode::JumpIfZero, 3, 0, 0, 14);

    code[11] = LabInstruction::new(Opcode::LoadMetric, 5, 0, MetricId::KernelPhase as i32, 0);

    code[12] = LabInstruction::new(Opcode::SetControl, 5, 0, 0, ControlField::TargetPhase as u8);

    code[13] = LabInstruction::new(Opcode::Halt, 0, 0, 0, 0);

    code[14] = LabInstruction::new(Opcode::Halt, 0, 0, 0, 0);

    LabProgram::new(code, 15)
};
