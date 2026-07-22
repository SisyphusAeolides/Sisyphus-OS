#![allow(unsafe_op_in_unsafe_fn)]
#![allow(dead_code, unused_variables, clippy::missing_safety_doc)]

use core::{
    arch::{asm, naked_asm},
    mem::size_of,
    ptr::{self, copy_nonoverlapping},
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};

// ============================================================================
// 1. HARDWARE BINDINGS & STUBS
// ============================================================================
pub mod plat {
    use super::*;

    pub const PAGE_SIZE: usize = 4096;
    pub const USER_TOP: u64 = 0x0000_7FFF_FFFF_F000;
    pub const USER_STACK_TOP: u64 = 0x0000_7FFF_FFFF_E000;
    pub const USER_STACK_PAGES: usize = 24;
    pub const USER_GUARD_PAGES: usize = 2;
    pub const USER_CS: u16 = 0x33; // Standard Ring 3 CS
    pub const USER_DS: u16 = 0x2B; // Standard Ring 3 DS
    pub const KERNEL_CS: u16 = 0x08;
    pub const KERNEL_DS: u16 = 0x10;

    pub const PTE_P: u64 = 1 << 0;
    pub const PTE_W: u64 = 1 << 1;
    pub const PTE_U: u64 = 1 << 2;
    pub const PTE_NX: u64 = 1 << 63;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum VmError { Oom, Map, Bad }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct AddressSpace {
        pub root_phys: u64,
        pub epoch: u64,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct TssRef {
        pub rsp0_slot: *mut u64,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct CpuLocal {
        pub cpu_id: u32,
        pub tss: TssRef,
        pub kernel_stack_top: u64,
    }

    static NEXT_FAKE_PHYS: AtomicU64 = AtomicU64::new(0x0020_0000);

    pub unsafe fn alloc_phys_page() -> Result<(u64, *mut u8), VmError> {
        let p = NEXT_FAKE_PHYS.fetch_add(PAGE_SIZE as u64, Ordering::AcqRel);
        Ok((p, p as *mut u8))
    }

    pub unsafe fn map_page(_aspace: &AddressSpace, _vaddr: u64, _paddr: u64, _flags: u64) -> Result<(), VmError> {
        // STUB: Wire this to your real page table walk
        Ok(())
    }

    pub unsafe fn new_user_address_space() -> Result<AddressSpace, VmError> {
        let (p, v) = alloc_phys_page()?;
        ptr::write_bytes(v, 0, PAGE_SIZE);
        Ok(AddressSpace { root_phys: p, epoch: 1 })
    }

    pub unsafe fn map_range_zeroed(
        aspace: &AddressSpace,
        start: u64,
        pages: usize,
        writable: bool,
        executable: bool,
    ) -> Result<(), VmError> {
        for i in 0..pages {
            let (p, v) = alloc_phys_page()?;
            ptr::write_bytes(v, 0, PAGE_SIZE);
            let mut f = PTE_P | PTE_U;
            if writable { f |= PTE_W; }
            if !executable { f |= PTE_NX; }
            map_page(aspace, start + (i as u64 * PAGE_SIZE as u64), p, f)?;
        }
        Ok(())
    }

    pub unsafe fn current_cpu_local() -> CpuLocal {
        static mut RSP0: u64 = 0;
        CpuLocal {
            cpu_id: 0,
            tss: TssRef { rsp0_slot: &raw mut RSP0 },
            kernel_stack_top: 0xFFFF_8000_0007_F000,
        }
    }

    pub unsafe fn write_tss_rsp0(cpu: &CpuLocal, rsp0: u64) {
        ptr::write_volatile(cpu.tss.rsp0_slot, rsp0);
    }

    pub unsafe fn load_cr3(root_phys: u64) {
        asm!("mov cr3, {}", in(reg) root_phys, options(nostack, preserves_flags));
    }

    pub unsafe fn rflags() -> u64 {
        let r: u64;
        asm!("pushfq; pop {}", out(reg) r, options(nomem, preserves_flags));
        r
    }
}

// ============================================================================
// 2. ELF LOADER (MINIMAL, FIXED-START, NO DYNAMIC LINKER)
// ============================================================================
pub mod elf64 {
    use super::*;

    pub const ET_EXEC: u16 = 2;
    pub const ET_DYN: u16 = 3;
    pub const EM_X86_64: u16 = 62;
    pub const PT_LOAD: u32 = 1;
    pub const PF_X: u32 = 1;
    pub const PF_W: u32 = 2;
    pub const PF_R: u32 = 4;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Ehdr {
        pub e_ident: [u8; 16],
        pub e_type: u16,
        pub e_machine: u16,
        pub e_version: u32,
        pub e_entry: u64,
        pub e_phoff: u64,
        pub e_shoff: u64,
        pub e_flags: u32,
        pub e_ehsize: u16,
        pub e_phentsize: u16,
        pub e_phnum: u16,
        pub e_shentsize: u16,
        pub e_shnum: u16,
        pub e_shstrndx: u16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Phdr {
        pub p_type: u32,
        pub p_flags: u32,
        pub p_offset: u64,
        pub p_vaddr: u64,
        pub p_paddr: u64,
        pub p_filesz: u64,
        pub p_memsz: u64,
        pub p_align: u64,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum ElfError {
        TooSmall, BadMagic, BadClass, BadEndian, BadMachine, BadType, BadPhentsize, Truncated,
    }

    pub unsafe fn parse_header(image: &[u8]) -> Result<&Ehdr, ElfError> {
        if image.len() < size_of::<Ehdr>() { return Err(ElfError::TooSmall); }
        let eh = &*(image.as_ptr() as *const Ehdr);
        if eh.e_ident[0..4] != [0x7F, b'E', b'L', b'F'] { return Err(ElfError::BadMagic); }
        if eh.e_ident[4] != 2 { return Err(ElfError::BadClass); } // 64-bit
        if eh.e_ident[5] != 1 { return Err(ElfError::BadEndian); } // LSB
        if eh.e_machine != EM_X86_64 { return Err(ElfError::BadMachine); }
        if eh.e_type != ET_EXEC && eh.e_type != ET_DYN { return Err(ElfError::BadType); }
        if eh.e_phentsize as usize != size_of::<Phdr>() { return Err(ElfError::BadPhentsize); }
        
        let end = eh.e_phoff as usize + eh.e_phnum as usize * size_of::<Phdr>();
        if end > image.len() { return Err(ElfError::Truncated); }
        Ok(eh)
    }

    pub unsafe fn phdrs<'a>(image: &'a [u8], eh: &Ehdr) -> Result<&'a [Phdr], ElfError> {
        Ok(core::slice::from_raw_parts(
            image.as_ptr().add(eh.e_phoff as usize) as *const Phdr,
            eh.e_phnum as usize,
        ))
    }
}

// ============================================================================
// 3. THE STRANGE SIGIL ABI
// ============================================================================
pub mod sigil {
    pub const SIGIL_LAUNCH: u64 = 0xA11CE_1000;
    pub const SIGIL_WRITE:  u64 = 0xA11CE_1001;
    pub const SIGIL_YIELD:  u64 = 0xA11CE_1002;
    pub const SIGIL_TIME:   u64 = 0xA11CE_1003;
    pub const SIGIL_REVOKE: u64 = 0xA11CE_10FE;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct SigilFrame {
        pub magic: u64,
        pub arg0: u64,
        pub arg1: u64,
        pub arg2: u64,
        pub arg3: u64,
        pub seal: u64,
    }

    pub fn seal(pid: u32, epoch: u64, entry: u64) -> u64 {
        (pid as u64)
            ^ epoch.rotate_left(17)
            ^ entry.rotate_right(9)
            ^ 0xD15C_A11C_E000_0001
    }
}

// ============================================================================
// 4. BLACKLAB SUBSTRATE & LAUNCH PATH
// ============================================================================
pub mod blacklab {
    use super::*;
    use super::{elf64::*, plat, sigil};

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Ownership {
        pub parent_pid: u32,
        pub owner_uid: u32,
        pub owner_gid: u32,
        pub session_id: u32,
        pub retained: u64,
        pub launch_epoch: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Process {
        pub pid: u32,
        pub epoch: u64,
        pub aspace: plat::AddressSpace,
        pub entry: u64,
        pub image_lo: u64,
        pub image_hi: u64,
        pub stack_top: u64,
        pub shadow_stack_top: u64,
        pub ownership: Ownership,
        pub launch_seal: u64,
        pub active: bool,
        pub poison: u64,
        pub journal_head: u64, // For deterministic replay mapping
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Thread {
        pub tid: u32,
        pub pid: u32,
        pub user_rip: u64,
        pub user_rsp: u64,
        pub user_ssp: u64,
        pub kernel_rsp0: u64,
        pub fs_base: u64,
        pub gs_base: u64,
        pub state: u32,
        pub sigil: u64,
    }

    static NEXT_PID: AtomicU32 = AtomicU32::new(1);
    static NEXT_TID: AtomicU32 = AtomicU32::new(1);
    static GLOBAL_EPOCH: AtomicU64 = AtomicU64::new(1);

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum LoadError { Elf(ElfError), Vm(plat::VmError), Poison, BadLaunch }

    // Minimal x86-64 hardware trap frame for IRETQ/SYSCALL
    #[repr(C, packed)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct TrapFrame {
        // Offset 0x00
        pub r15: u64, pub r14: u64, pub r13: u64, pub r12: u64,
        // Offset 0x20
        pub r11: u64, pub r10: u64, pub r9: u64,  pub r8: u64,
        // Offset 0x40
        pub rbp: u64, pub rdi: u64, pub rsi: u64, pub rdx: u64,
        // Offset 0x60
        pub rcx: u64, pub rbx: u64, pub rax: u64,
        // Offset 0x78
        pub error_code: u64,
        // Offset 0x80 (IRETQ Frame)
        pub rip: u64,
        pub cs: u64,
        pub rflags: u64,
        pub rsp: u64,
        pub ss: u64,
    }

    impl Default for TrapFrame {
        fn default() -> Self { unsafe { core::mem::zeroed() } }
    }

    pub fn next_epoch() -> u64 { GLOBAL_EPOCH.fetch_add(1, Ordering::AcqRel) }

    fn build_stack(top: u64) -> u64 {
        let mut sp = top & !0xF;
        sp -= 8; unsafe { *(sp as *mut u64) = 0; } // Auxv end / env end
        sp -= 8; unsafe { *(sp as *mut u64) = 0; } // argv end
        sp -= 8; unsafe { *(sp as *mut u64) = 0; } // argc = 0
        sp
    }

    unsafe fn map_segment(aspace: &plat::AddressSpace, image: &[u8], ph: &Phdr) -> Result<(), LoadError> {
        if ph.p_memsz < ph.p_filesz { return Err(LoadError::BadLaunch); }
        let start = ph.p_vaddr & !((plat::PAGE_SIZE as u64) - 1);
        let end = (ph.p_vaddr + ph.p_memsz + (plat::PAGE_SIZE as u64 - 1)) & !((plat::PAGE_SIZE as u64) - 1);
        let pages = ((end - start) / plat::PAGE_SIZE as u64) as usize;
        
        let writable = (ph.p_flags & PF_W) != 0;
        let executable = (ph.p_flags & PF_X) != 0;
        
        plat::map_range_zeroed(aspace, start, pages, writable, executable).map_err(LoadError::Vm)?;
        
        if ph.p_filesz != 0 {
            let src = image.as_ptr().add(ph.p_offset as usize);
            let dst = ph.p_vaddr as *mut u8;
            copy_nonoverlapping(src, dst, ph.p_filesz as usize);
        }
        Ok(())
    }

    pub unsafe fn load(image: &[u8]) -> Result<(Process, Thread), LoadError> {
        let eh = parse_header(image).map_err(LoadError::Elf)?;
        let ph = phdrs(image, eh).map_err(LoadError::Elf)?;
        let aspace = plat::new_user_address_space().map_err(LoadError::Vm)?;

        let pid = NEXT_PID.fetch_add(1, Ordering::AcqRel);
        let epoch = next_epoch();
        let mut lo = u64::MAX;
        let mut hi = 0u64;

        for p in ph {
            if p.p_type != PT_LOAD { continue; }
            if p.p_vaddr >= plat::USER_TOP { return Err(LoadError::BadLaunch); }
            map_segment(&aspace, image, p)?;
            lo = lo.min(p.p_vaddr & !((plat::PAGE_SIZE as u64) - 1));
            hi = hi.max((p.p_vaddr + p.p_memsz + (plat::PAGE_SIZE as u64 - 1)) & !((plat::PAGE_SIZE as u64) - 1));
        }

        // Establish stack and shadow-stack zones.
        let user_stack_top = plat::USER_STACK_TOP;
        let shadow_stack_top = plat::USER_STACK_TOP - ((plat::USER_STACK_PAGES as u64 + plat::USER_GUARD_PAGES as u64) * plat::PAGE_SIZE as u64);
        
        plat::map_range_zeroed(&aspace, shadow_stack_top - (plat::USER_STACK_PAGES as u64 * plat::PAGE_SIZE as u64), plat::USER_STACK_PAGES, true, false).map_err(LoadError::Vm)?;
        plat::map_range_zeroed(&aspace, user_stack_top - (plat::USER_STACK_PAGES as u64 * plat::PAGE_SIZE as u64), plat::USER_STACK_PAGES, true, false).map_err(LoadError::Vm)?;

        let launch_seal = sigil::seal(pid, epoch, eh.e_entry);
        let process = Process {
            pid, epoch, aspace, entry: eh.e_entry, image_lo: lo, image_hi: hi,
            stack_top: build_stack(user_stack_top), shadow_stack_top,
            ownership: Ownership {
                parent_pid: 0, owner_uid: 0, owner_gid: 0, session_id: pid,
                retained: 1, launch_epoch: epoch,
            },
            launch_seal, active: true, poison: 0, journal_head: 0,
        };

        let thread = Thread {
            tid: NEXT_TID.fetch_add(1, Ordering::AcqRel), pid,
            user_rip: process.entry, user_rsp: process.stack_top, user_ssp: process.shadow_stack_top,
            kernel_rsp0: plat::current_cpu_local().kernel_stack_top,
            fs_base: 0, gs_base: 0, state: 1, sigil: launch_seal,
        };

        Ok((process, thread))
    }

    pub fn make_user_tf(proc: &Process, thread: &Thread) -> TrapFrame {
        let mut tf = TrapFrame::default();
        tf.rip = proc.entry;
        tf.cs = plat::USER_CS as u64;
        tf.rflags = unsafe { plat::rflags() } | (1 << 9); // Interrupts enabled (IF)
        tf.rsp = thread.user_rsp;
        tf.ss = plat::USER_DS as u64;
        
        // Pass the sigil to user-space in rax to authorize the launch.
        tf.rax = proc.launch_seal;
        tf
    }

    pub fn syscall_gate(tf: &mut TrapFrame, proc: &mut Process, thread: &mut Thread) -> i64 {
        // Validate process state and sigil seal
        if thread.sigil != proc.launch_seal || !proc.active {
            // Actively poison the state on violation
            proc.poison ^= 0xDEAD_BEEF_DEAD_BEEF;
            return -1;
        }

        // Extremely narrow and strange ABI based on sigils
        match tf.rax {
            sigil::SIGIL_WRITE => {
                let fd = tf.rdi;
                let _buf = tf.rsi as *const u8;
                let len = tf.rdx as usize;
                // Basic STDOUT / STDERR write stub
                if fd == 1 || fd == 2 { len as i64 } else { -1 }
            }
            sigil::SIGIL_TIME => super::rdtsc() as i64,
            sigil::SIGIL_YIELD => {
                // Yield execution context marker
                0
            }
            sigil::SIGIL_REVOKE => {
                // The process revokes its own seal
                proc.active = false;
                proc.poison = proc.poison.wrapping_add(1);
                -2
            }
            _ => {
                // Unknown sigil -> Immediate process taint
                proc.poison ^= 0xBAD_C0DE_BAD_C0DE;
                -38 // ENOSYS
            }
        }
    }

    // A naked function implementing a raw Ring 3 entry via IRETQ.
    // WARNING: This assumes `rdi` holds a pointer to a fully constructed `TrapFrame`.
    #[unsafe(naked)]
    pub unsafe extern "C" fn enter_ring3(_tf: *const TrapFrame) -> ! {
        naked_asm!(
            // Move pointer to the TrapFrame into a less clobbered register
            "mov rdx, rdi",
            
            // Restore GPRs from offsets based on TrapFrame layout
            "mov r15, [rdx + 0x00]",
            "mov r14, [rdx + 0x08]",
            "mov r13, [rdx + 0x10]",
            "mov r12, [rdx + 0x18]",
            "mov r11, [rdx + 0x20]",
            "mov r10, [rdx + 0x28]",
            "mov r9,  [rdx + 0x30]",
            "mov r8,  [rdx + 0x38]",
            "mov rbp, [rdx + 0x40]",
            "mov rdi, [rdx + 0x48]",
            "mov rsi, [rdx + 0x50]",
            // Skip rdx [0x58] for now, it's our base pointer
            "mov rcx, [rdx + 0x60]",
            "mov rbx, [rdx + 0x68]",
            "mov rax, [rdx + 0x70]",
            
            // Construct the IRETQ stack frame (SS, RSP, RFLAGS, CS, RIP)
            "push qword ptr [rdx + 0xA0]", // SS
            "push qword ptr [rdx + 0x98]", // RSP
            "push qword ptr [rdx + 0x90]", // RFLAGS
            "push qword ptr [rdx + 0x88]", // CS
            "push qword ptr [rdx + 0x80]", // RIP
            
            // Finally restore RDX
            "mov rdx, [rdx + 0x58]",
            
            // Blast off into Ring 3
            "iretq",
        )
    }
}

#[inline(always)]
pub fn rdtsc() -> u64 {
    unsafe {
        let lo: u32;
        let hi: u32;
        asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
        ((hi as u64) << 32) | (lo as u64)
    }
}

// Top-level harness to wire it up.
pub unsafe fn launch_blacklab(image: &[u8]) -> Result<blacklab::TrapFrame, blacklab::LoadError> {
    let (proc, thread) = blacklab::load(image)?;
    let cpu = plat::current_cpu_local();
    
    // Set up the Ring 0 stack pointer for when the user process traps.
    plat::write_tss_rsp0(&cpu, thread.kernel_rsp0);
    
    // Install the user address space.
    plat::load_cr3(proc.aspace.root_phys);
    
    // Return the trap frame ready for `enter_ring3`
    Ok(blacklab::make_user_tf(&proc, &thread))
}

pub fn blacklab_pulse() -> u64 {
    let epoch = blacklab::next_epoch();
    epoch ^ rdtsc().rotate_left(13) ^ 0xB1A9_7B1A_7B1A_0001
}
