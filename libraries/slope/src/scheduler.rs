use aether::grimoire;

use crate::{SyscallError, syscall};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Priority {
    Idle = 0,
    Background = 1,
    Normal = 2,
    Interactive = 3,
    Critical = 4,
    Nexus = 5,
}

impl Priority {
    pub const fn mass(self) -> u16 {
        match self {
            Self::Idle => 0x1000,
            Self::Background => 0x3000,
            Self::Normal => 0x6000,
            Self::Interactive => 0x9000,
            Self::Critical => 0xd000,
            Self::Nexus => 0xffff,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhaseHint {
    pub phase_bin: u16,
    pub coherence: u16,
    pub priority: Priority,
    pub flags: u16,
}

impl PhaseHint {
    pub const fn packed(self) -> u64 {
        (self.phase_bin as u64 & 0x03ff)
            | ((self.coherence as u64 & 0x03ff) << 10)
            | ((self.priority.mass() as u64) << 20)
            | ((self.flags as u64) << 36)
    }
}

pub fn yield_with_hint(hint: PhaseHint) -> Result<(), SyscallError> {
    // SAFETY: Every argument is scalar and follows Slope's syscall ABI.
    unsafe {
        syscall(
            grimoire::SYS_YIELD,
            [hint.packed() as usize, 0, 0, 0, 0, 0],
        )
    }
    .map(|_| ())
}

pub fn set_priority(priority: Priority) -> Result<(), SyscallError> {
    // SAFETY: Every argument is scalar and follows Slope's syscall ABI.
    unsafe {
        syscall(
            grimoire::SYS_SETPRIO,
            [usize::from(priority.mass()), 0, 0, 0, 0, 0],
        )
    }
    .map(|_| ())
}

pub fn sleep_ns(nanoseconds: u64) -> Result<(), SyscallError> {
    let low = nanoseconds as u32 as usize;
    let high = (nanoseconds >> 32) as u32 as usize;

    // SAFETY: The duration is split into two scalar words.
    unsafe {
        syscall(
            grimoire::SYS_CLOCK_SLEEP,
            [low, high, 0, 0, 0, 0],
        )
    }
    .map(|_| ())
}
