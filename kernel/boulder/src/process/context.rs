//! Architecture-visible state required to resume an x86-64 Ring 3 process.
//!
//! The register prefix deliberately matches the order used by Boulder's
//! interrupt entry frame. Address-space and kernel-stack ownership stay
//! outside [`SavedUserContext`]: those values are selected from trusted
//! lifecycle metadata rather than copied from a user-controlled entry frame.

pub const USER_ADDRESS_MINIMUM: u64 = 0x1000;
pub const USER_ADDRESS_LIMIT: u64 = 0x0000_8000_0000_0000;
pub const KERNEL_ADDRESS_MINIMUM: u64 = 0xffff_8000_0000_0000;
pub const INITIAL_USER_FLAGS: u64 = (1 << 1) | (1 << 9);

const PAGE_MASK: u64 = 0xfff;
const PHYSICAL_ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;
const USER_FLAGS_ALLOWED: u64 = (1 << 0)
    | (1 << 1)
    | (1 << 2)
    | (1 << 4)
    | (1 << 6)
    | (1 << 7)
    | (1 << 8)
    | (1 << 9)
    | (1 << 10)
    | (1 << 11)
    | (1 << 16)
    | (1 << 18)
    | (1 << 21);

/// Complete general-purpose state at a user-to-kernel boundary.
///
/// `instruction_pointer`, `flags`, and `stack_pointer` are the architectural
/// return triple. On a `SYSCALL` entry, `rcx` and `r11` contain copies of the
/// return instruction pointer and flags because the instruction itself
/// clobbers those registers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C, align(16))]
pub struct SavedUserContext {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub instruction_pointer: u64,
    pub flags: u64,
    pub stack_pointer: u64,
}

impl SavedUserContext {
    pub const EMPTY: Self = Self {
        r15: 0,
        r14: 0,
        r13: 0,
        r12: 0,
        r11: 0,
        r10: 0,
        r9: 0,
        r8: 0,
        rbp: 0,
        rdi: 0,
        rsi: 0,
        rdx: 0,
        rcx: 0,
        rbx: 0,
        rax: 0,
        instruction_pointer: 0,
        flags: 0,
        stack_pointer: 0,
    };

    /// Builds the first context dispatched for a newly installed process.
    pub const fn initial(instruction_pointer: u64, stack_pointer: u64) -> Self {
        Self {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            r11: INITIAL_USER_FLAGS,
            r10: 0,
            r9: 0,
            r8: 0,
            rbp: 0,
            rdi: 0,
            rsi: 0,
            rdx: 0,
            rcx: instruction_pointer,
            rbx: 0,
            rax: 0,
            instruction_pointer,
            flags: INITIAL_USER_FLAGS,
            stack_pointer,
        }
    }

    pub const fn validate(self) -> Result<(), ContextError> {
        if !valid_user_address(self.instruction_pointer) {
            return Err(ContextError::InvalidInstructionPointer);
        }
        if !valid_user_address(self.stack_pointer) {
            return Err(ContextError::InvalidStackPointer);
        }
        if self.flags & (1 << 1) == 0 || self.flags & !USER_FLAGS_ALLOWED != 0 {
            return Err(ContextError::InvalidFlags);
        }
        Ok(())
    }

    /// Sets the value observed in RAX when this context resumes after a
    /// syscall. RCX and R11 remain the architectural `SYSCALL` clobbers.
    pub const fn set_syscall_result(&mut self, result: isize) {
        self.rax = result as u64;
    }

    /// Returns the six scalar arguments in Boulder's native syscall order.
    pub const fn syscall_arguments(self) -> [u64; 6] {
        [self.rdi, self.rsi, self.rdx, self.r10, self.r8, self.r9]
    }
}

/// Trusted dispatch state consumed by the architecture switch path.
///
/// The lifecycle table constructs this value from its saved user registers
/// and immutable launch metadata. Assembly must load CR3 from
/// `address_space_root`, publish `kernel_stack_pointer` to the current CPU's
/// TSS RSP0, and only then resume `user`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C, align(16))]
pub struct DispatchContext {
    pub user: SavedUserContext,
    pub address_space_root: u64,
    pub kernel_stack_pointer: u64,
}

impl DispatchContext {
    pub const fn validate(self) -> Result<(), ContextError> {
        if let Err(error) = self.user.validate() {
            return Err(error);
        }
        if !valid_page_table_root(self.address_space_root) {
            return Err(ContextError::InvalidAddressSpaceRoot);
        }
        if !valid_kernel_stack_pointer(self.kernel_stack_pointer) {
            return Err(ContextError::InvalidKernelStackPointer);
        }
        Ok(())
    }
}

/// A lifecycle-issued authority to cross the final kernel-to-user boundary.
///
/// The process identity and scheduler epoch are deliberately adjacent to the
/// machine context. Assembly passes this object back to Rust immediately
/// before switching TSS RSP0 and CR3, so a recycled PID or superseded
/// scheduling decision cannot authorize a return.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C, align(16))]
pub struct AuthorizedUserReturn {
    pub dispatch: DispatchContext,
    pub pid: u32,
    pub generation: u32,
    pub scheduler_epoch: u64,
}

impl AuthorizedUserReturn {
    pub const EMPTY: Self = Self {
        dispatch: DispatchContext {
            user: SavedUserContext::EMPTY,
            address_space_root: 0,
            kernel_stack_pointer: 0,
        },
        pid: 0,
        generation: 0,
        scheduler_epoch: 0,
    };

    pub const fn validate(self) -> Result<(), ContextError> {
        if self.pid == 0 || self.generation == 0 || self.scheduler_epoch == 0 {
            return Err(ContextError::InvalidDispatchAuthority);
        }
        self.dispatch.validate()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextError {
    InvalidInstructionPointer,
    InvalidStackPointer,
    InvalidFlags,
    InvalidAddressSpaceRoot,
    InvalidKernelStackPointer,
    InvalidDispatchAuthority,
}

pub const fn valid_user_address(address: u64) -> bool {
    address >= USER_ADDRESS_MINIMUM && address < USER_ADDRESS_LIMIT
}

pub const fn valid_page_table_root(root: u64) -> bool {
    root != 0 && root & PAGE_MASK == 0 && root & !PHYSICAL_ADDRESS_MASK == 0
}

pub const fn valid_kernel_stack_pointer(pointer: u64) -> bool {
    pointer >= KERNEL_ADDRESS_MINIMUM && pointer & 0xf == 0
}

const _: () = assert!(core::mem::size_of::<SavedUserContext>() == 144);
const _: () = assert!(core::mem::align_of::<SavedUserContext>() == 16);
const _: () = assert!(core::mem::offset_of!(SavedUserContext, r15) == 0);
const _: () = assert!(core::mem::offset_of!(SavedUserContext, rax) == 112);
const _: () = assert!(core::mem::offset_of!(SavedUserContext, instruction_pointer) == 120);
const _: () = assert!(core::mem::offset_of!(SavedUserContext, flags) == 128);
const _: () = assert!(core::mem::offset_of!(SavedUserContext, stack_pointer) == 136);
const _: () = assert!(core::mem::size_of::<DispatchContext>() == 160);
const _: () = assert!(core::mem::offset_of!(DispatchContext, address_space_root) == 144);
const _: () = assert!(core::mem::offset_of!(DispatchContext, kernel_stack_pointer) == 152);
const _: () = assert!(core::mem::size_of::<AuthorizedUserReturn>() == 176);
const _: () = assert!(core::mem::align_of::<AuthorizedUserReturn>() == 16);
const _: () = assert!(core::mem::offset_of!(AuthorizedUserReturn, dispatch) == 0);
const _: () = assert!(core::mem::offset_of!(AuthorizedUserReturn, pid) == 160);
const _: () = assert!(core::mem::offset_of!(AuthorizedUserReturn, generation) == 164);
const _: () = assert!(core::mem::offset_of!(AuthorizedUserReturn, scheduler_epoch) == 168);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_context_is_a_valid_zeroed_user_dispatch() {
        let user = SavedUserContext::initial(0x2000, 0x8000);
        assert_eq!(user.validate(), Ok(()));
        assert_eq!(user.rcx, user.instruction_pointer);
        assert_eq!(user.r11, user.flags);
        assert_eq!(user.flags, INITIAL_USER_FLAGS);
        assert_eq!(user.rax, 0);

        let dispatch = DispatchContext {
            user,
            address_space_root: 0x3000,
            kernel_stack_pointer: 0xffff_8000_0000_4000,
        };
        assert_eq!(dispatch.validate(), Ok(()));
    }

    #[test]
    fn rejects_non_user_returns_privileged_flags_and_untrusted_switch_state() {
        let valid = SavedUserContext::initial(0x2000, 0x8000);

        assert_eq!(
            SavedUserContext {
                instruction_pointer: USER_ADDRESS_LIMIT,
                ..valid
            }
            .validate(),
            Err(ContextError::InvalidInstructionPointer),
        );
        assert_eq!(
            SavedUserContext {
                stack_pointer: 0,
                ..valid
            }
            .validate(),
            Err(ContextError::InvalidStackPointer),
        );
        assert_eq!(
            SavedUserContext {
                flags: valid.flags | (3 << 12),
                ..valid
            }
            .validate(),
            Err(ContextError::InvalidFlags),
        );
        assert_eq!(
            DispatchContext {
                user: valid,
                address_space_root: 0x3123,
                kernel_stack_pointer: 0xffff_8000_0000_4000,
            }
            .validate(),
            Err(ContextError::InvalidAddressSpaceRoot),
        );
        assert_eq!(
            DispatchContext {
                user: valid,
                address_space_root: 0x3000,
                kernel_stack_pointer: 0x8000,
            }
            .validate(),
            Err(ContextError::InvalidKernelStackPointer),
        );

        assert_eq!(
            AuthorizedUserReturn {
                dispatch: DispatchContext {
                    user: valid,
                    address_space_root: 0x3000,
                    kernel_stack_pointer: 0xffff_8000_0000_4000,
                },
                pid: 0,
                generation: 1,
                scheduler_epoch: 1,
            }
            .validate(),
            Err(ContextError::InvalidDispatchAuthority),
        );
    }
}
