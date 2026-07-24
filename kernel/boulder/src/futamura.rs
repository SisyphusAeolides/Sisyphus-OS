// kernel/boulder/src/futamura.rs
//! Futamura-1 — first projection specializer for syscall dispatch
//!
//! Generic interpreter:
//!   loop { op = decode(); match op { ... check caps ... } }
//!
//! Frozen program = capability bitvector + allowed syscall bitmap.
//! Residual = dense table of only permitted ops with cap checks eliminated
//! when the bit is statically known set.
//!
//! This is partial evaluation, not JIT codegen: we emit a residual
//! descriptor the assembly stub interprets in a tight straight-line form.


pub const MAX_SYS: usize = 64;
pub const MAX_RESIDUAL: usize = 48;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum SysOp {
    Nop = 0,
    Send = 1,
    Recv = 2,
    Map = 3,
    Unmap = 4,
    Grant = 5,
    Yield = 6,
    OpenSession = 7,
    DmaAlloc = 8,
    IrqRegister = 9,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapVector {
    /// bit i = owns Right i (mirrors capability.rs rights)
    pub bits: u64,
    pub allowed_sys: u64,
}

impl CapVector {
    pub const fn empty() -> Self {
        Self {
            bits: 0,
            allowed_sys: 0,
        }
    }

    pub fn allows_sys(self, op: SysOp) -> bool {
        let i = op as u64;
        (self.allowed_sys >> i) & 1 == 1
    }

    pub fn has_cap(self, cap_bit: u8) -> bool {
        (self.bits >> cap_bit) & 1 == 1
    }
}

/// What a residual entry needs at runtime (args still dynamic).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResidualOp {
    pub op: SysOp,
    /// If true, cap_bit was static-yes and check is deleted
    pub cap_eliminated: bool,
    pub cap_bit: u8,
    /// Static grant mask folded from CapVector
    pub folded_grant_mask: u64,
}

impl ResidualOp {
    pub const NOP: Self = Self {
        op: SysOp::Nop,
        cap_eliminated: true,
        cap_bit: 0,
        folded_grant_mask: 0,
    };
}

#[derive(Clone, Copy, Debug)]
pub struct ResidualProgram {
    pub ops: [ResidualOp; MAX_RESIDUAL],
    pub len: u8,
    pub source_caps: CapVector,
    /// Hash for authenticity (bootstrap chronal can pin this)
    pub hologram: u64,
}

impl ResidualProgram {
    pub const fn empty() -> Self {
        Self {
            ops: [ResidualOp::NOP; MAX_RESIDUAL],
            len: 0,
            source_caps: CapVector::empty(),
            hologram: 0,
        }
    }
}

/// Generic interpreter table: syscall → required cap bit (255 = none)
pub struct InterpreterSpec {
    pub req_cap: [u8; MAX_SYS],
}

impl InterpreterSpec {
    pub const fn boulder_default() -> Self {
        let mut req = [255u8; MAX_SYS];
        req[SysOp::Send as usize] = 0; // fabric
        req[SysOp::Recv as usize] = 0;
        req[SysOp::Map as usize] = 1; // phys mem
        req[SysOp::Unmap as usize] = 1;
        req[SysOp::Grant as usize] = 5; // policy
        req[SysOp::DmaAlloc as usize] = 3; // dma
        req[SysOp::IrqRegister as usize] = 2; // device
        req[SysOp::OpenSession as usize] = 0;
        req[SysOp::Yield as usize] = 255;
        Self { req_cap: req }
    }
}

/// First Futamura projection: mix interpreter with frozen caps.
pub fn specialize(interp: &InterpreterSpec, caps: CapVector) -> ResidualProgram {
    let mut residual = ResidualProgram::empty();
    residual.source_caps = caps;
    let mut out = 0usize;

    // Partial eval: only emit syscalls allowed; fold cap checks.
    let mut sys_i = 1u16; // skip Nop
    while sys_i < MAX_SYS as u16 && out < MAX_RESIDUAL {
        let op = match sys_i {
            1 => SysOp::Send,
            2 => SysOp::Recv,
            3 => SysOp::Map,
            4 => SysOp::Unmap,
            5 => SysOp::Grant,
            6 => SysOp::Yield,
            7 => SysOp::OpenSession,
            8 => SysOp::DmaAlloc,
            9 => SysOp::IrqRegister,
            _ => SysOp::Nop,
        };
        if op as u16 != sys_i {
            sys_i += 1;
            continue;
        }
        if !caps.allows_sys(op) {
            sys_i += 1;
            continue;
        }
        let req = interp.req_cap[op as usize];
        let (elim, ok) = if req == 255 {
            (true, true)
        } else if caps.has_cap(req) {
            (true, true) // static knowledge: will always pass
        } else {
            (false, false) // statically impossible — dead code eliminate
        };
        if !ok {
            sys_i += 1;
            continue;
        }
        residual.ops[out] = ResidualOp {
            op,
            cap_eliminated: elim,
            cap_bit: req,
            folded_grant_mask: caps.bits,
        };
        out += 1;
        sys_i += 1;
    }
    residual.len = out as u8;
    residual.hologram = hologram_of(&residual);
    residual
}

fn hologram_of(r: &ResidualProgram) -> u64 {
    let mut h = 0xF17A_0000_u64 ^ r.source_caps.bits ^ r.source_caps.allowed_sys;
    for i in 0..r.len as usize {
        let o = r.ops[i];
        h = h.rotate_left(7)
            ^ ((o.op as u64) << 32)
            ^ ((o.cap_eliminated as u64) << 16)
            ^ o.folded_grant_mask;
    }
    h
}

/// Residual executor — branch-light.
pub fn exec_residual(r: &ResidualProgram, op: SysOp, dynamic_ok: bool) -> bool {
    for i in 0..r.len as usize {
        if r.ops[i].op != op {
            continue;
        }
        if r.ops[i].cap_eliminated {
            return true;
        }
        return dynamic_ok;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eliminates_static_caps() {
        let mut caps = CapVector::empty();
        caps.bits = (1 << 0) | (1 << 1); // fabric + phys
        caps.allowed_sys = (1 << SysOp::Send as u64) | (1 << SysOp::Map as u64);
        let r = specialize(&InterpreterSpec::boulder_default(), caps);
        assert!(r.len >= 2);
        assert!(r.ops.iter().take(r.len as usize).all(|o| o.cap_eliminated));
        assert!(exec_residual(&r, SysOp::Send, false));
        assert!(!exec_residual(&r, SysOp::DmaAlloc, true));
    }
}
