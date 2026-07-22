use crate::many_worlds::{
    ManyWorlds, SchedulerPolicy,
};

pub static SCHEDULER_WORLDS:
    crate::sync::SpinLock<ManyWorlds<4>> =
    crate::sync::SpinLock::new(ManyWorlds::new([
        SchedulerPolicy {
            latency_weight: 0x0180,
            throughput_weight: 0x0100,
            heat_weight: 0x0080,
            fairness_weight: 0x0100,
            quantum_ticks: 64,
            priority_mass_ceiling: 0xb000,
        },
        SchedulerPolicy {
            latency_weight: 0x0100,
            throughput_weight: 0x0200,
            heat_weight: 0x0100,
            fairness_weight: 0x0080,
            quantum_ticks: 256,
            priority_mass_ceiling: 0xe000,
        },
        SchedulerPolicy {
            latency_weight: 0x0080,
            throughput_weight: 0x0100,
            heat_weight: 0x0200,
            fairness_weight: 0x0180,
            quantum_ticks: 128,
            priority_mass_ceiling: 0x9000,
        },
        SchedulerPolicy {
            latency_weight: 0x0140,
            throughput_weight: 0x0140,
            heat_weight: 0x0140,
            fairness_weight: 0x0140,
            quantum_ticks: 96,
            priority_mass_ceiling: 0xc000,
        },
    ]));
