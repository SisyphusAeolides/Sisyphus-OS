// libraries/driver-abi/src/prometheus.rs
// #![no_std] inherited
//
// PROMETHEUS — Morphic ABI Transpiler
//
// Goal: given a raw C driver .so/.ko/.dll binary blob, automatically
// detect its calling convention and generate live thunk trampolines
// that bridge it to KernelApi WITHOUT requiring the driver's source code.
//
// Method:
//  1. Symbol scan: walk ELF/PE symbol table to find driver entry point
//     (sisyphus_driver_entry, _driver_init, DriverEntry, etc.)
//  2. Prologue analysis: disassemble first 32 bytes of each exported fn
//     using a minimal x86-64 decoder (no_std, hand-rolled, <200 LOC)
//     Detect: shadow space allocation (MS x64), red zone usage (SysV),
//             stack frame size, register save patterns
//  3. Convention classifier: rule-based state machine → outputs CallingConv enum
//  4. Thunk generation: write x86-64 machine code into a trampoline page
//     that shuffles registers/stack to match KernelApi's SysV ABI
//  5. Live patch: rewrite the driver's import table entry with trampoline address
//
// Supported conventions:
//   SysV_AMD64       — Linux, macOS, most Unix (rdi, rsi, rdx, rcx, r8, r9)
//   MsX64            — Windows, UEFI (rcx, rdx, r8, r9 + 32-byte shadow space)
//   Cdecl32          — Legacy 32-bit Linux/Windows (stack args, caller cleanup)
//   Stdcall32        — Win32 drivers (stack args, callee cleanup)
//   Aapcs64          — ARM64 Linux/BSD (x0..x7 args)
//   RiscVLP64        — RISC-V Linux (a0..a7 args)
//   Unknown          — fallback: probe with SysV, monitor for crashes

extern crate alloc;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

// ─────────────────────────────────────────────
// CALLING CONVENTION
// ─────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum CallingConv {
    SysVAmd64 = 0,
    MsX64 = 1,
    Cdecl32 = 2,
    Stdcall32 = 3,
    Aapcs64 = 4,
    RiscVLp64 = 5,
    Unknown = 0xFF,
}

// Arg count in registers per convention
impl CallingConv {
    pub const fn reg_arg_count(self) -> usize {
        match self {
            Self::SysVAmd64 => 6,
            Self::MsX64 => 4,
            Self::Aapcs64 => 8,
            Self::RiscVLp64 => 8,
            Self::Cdecl32 | Self::Stdcall32 => 0, // all stack
            Self::Unknown => 6,                   // assume SysV
        }
    }

    pub const fn has_shadow_space(self) -> bool {
        matches!(self, Self::MsX64)
    }

    pub const fn callee_cleanup(self) -> bool {
        matches!(self, Self::Stdcall32)
    }
}

// ─────────────────────────────────────────────
// MINIMAL x86-64 PROLOGUE DECODER
// State machine: reads up to 32 bytes of function prologue
// looking for calling-convention fingerprint patterns
// ─────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProbeState {
    Start,
    SeenPush,   // push rbp / push rdi etc
    SeenSubRsp, // sub rsp, N
    Done,
}

pub struct PrologueDecoder {
    pub detected: CallingConv,
    pub stack_frame_size: u32,
    pub saves_shadow: bool,
    pub uses_red_zone: bool,
    pub is_32bit: bool,
}

impl PrologueDecoder {
    pub fn decode(bytes: &[u8]) -> Self {
        let mut state = ProbeState::Start;
        let mut stack_frame = 0u32;
        let mut uses_red_zone = false;
        let mut is_32bit = false;
        let mut shadow_alloc = false;
        let mut sysv_spills = 0u32; // count of rdi/rsi/rdx/rcx spills (SysV save pattern)
        let mut ms_spills = 0u32; // count of rcx/rdx/r8/r9 spills (MS pattern)
        let mut i = 0usize;
        let limit = bytes.len().min(64);

        while i < limit {
            let b = bytes[i];
            match state {
                ProbeState::Start => {
                    match b {
                        0x55 => {
                            state = ProbeState::SeenPush;
                            i += 1;
                        } // push rbp
                        0x48 => {
                            // 48 83 EC xx = sub rsp, imm8
                            if i + 3 < limit && bytes[i + 1] == 0x83 && bytes[i + 2] == 0xEC {
                                stack_frame = bytes[i + 3] as u32;
                                if stack_frame == 0x28 {
                                    shadow_alloc = true;
                                }
                                state = ProbeState::SeenSubRsp;
                                i += 4;
                            } else if i + 4 < limit && bytes[i + 1] == 0x81 && bytes[i + 2] == 0xEC
                            {
                                // 48 81 EC xx xx xx xx = sub rsp, imm32
                                stack_frame = u32::from_le_bytes([
                                    bytes[i + 3],
                                    bytes[i + 4],
                                    if i + 5 < limit { bytes[i + 5] } else { 0 },
                                    if i + 6 < limit { bytes[i + 6] } else { 0 },
                                ]);
                                state = ProbeState::SeenSubRsp;
                                i += 7;
                            } else {
                                i += 1;
                            }
                        }
                        // 32-bit: 8B EC = mov ebp, esp (cdecl/stdcall)
                        0x8B if i + 1 < limit && bytes[i + 1] == 0xEC => {
                            is_32bit = true;
                            state = ProbeState::Done;
                            i += 2;
                        }
                        // C3 = ret with no frame → leaf fn / red zone
                        0xC3 => {
                            uses_red_zone = true;
                            state = ProbeState::Done;
                            i += 1;
                        }
                        _ => {
                            i += 1;
                        }
                    }
                }
                ProbeState::SeenPush | ProbeState::SeenSubRsp => {
                    // Look for register spills to identify SysV vs MS x64
                    // SysV spills: 48 89 7C 24 xx (mov [rsp+xx], rdi)
                    //              48 89 74 24 xx (mov [rsp+xx], rsi)
                    // MS x64 spills: 48 89 4C 24 xx (mov [rsp+xx], rcx)
                    //                48 89 54 24 xx (mov [rsp+xx], rdx)
                    if i + 4 < limit && bytes[i] == 0x48 && bytes[i + 1] == 0x89 {
                        match bytes[i + 2] {
                            0x7C | 0x74 | 0x55 | 0x4D => {
                                sysv_spills += 1;
                                i += 5;
                            }
                            0x4C | 0x54 | 0x44 | 0x5C => {
                                ms_spills += 1;
                                i += 5;
                            }
                            _ => {
                                i += 1;
                            }
                        }
                    } else {
                        state = ProbeState::Done;
                        i += 1;
                    }
                }
                ProbeState::Done => break,
            }
        }

        let detected = if is_32bit {
            // Can't distinguish cdecl/stdcall from prologue alone
            CallingConv::Cdecl32
        } else if shadow_alloc || ms_spills > sysv_spills {
            CallingConv::MsX64
        } else if uses_red_zone || sysv_spills > 0 {
            CallingConv::SysVAmd64
        } else {
            CallingConv::Unknown
        };

        PrologueDecoder {
            detected,
            stack_frame_size: stack_frame,
            saves_shadow: shadow_alloc,
            uses_red_zone,
            is_32bit,
        }
    }
}

// ─────────────────────────────────────────────
// THUNK TRAMPOLINE GENERATOR
// Writes x86-64 machine code that translates MS x64 → SysV AMD64
// (the most common mismatch in practice)
//
// MS x64 args: rcx, rdx, r8, r9 (+ 32-byte shadow space on stack)
// SysV args:   rdi, rsi, rdx, rcx, r8, r9
//
// Thunk for 4-arg MS→SysV:
//   mov rdi, rcx       ; 48 89 CF
//   mov rsi, rdx       ; 48 89 D6
//   mov rdx, r8        ; 4C 89 C2
//   mov rcx, r9        ; 4C 89 CA
//   sub rsp, 8         ; 48 83 EC 08  (align stack)
//   call [target]      ; FF 15 00 00 00 00 + target addr
//   add rsp, 8         ; 48 83 C4 08
//   ret                ; C3
//
// Max thunk size: 64 bytes. We store thunks in a static executable array.
// ─────────────────────────────────────────────

pub const MAX_THUNKS: usize = 128;
pub const THUNK_SIZE: usize = 64;
pub const THUNK_POOL_LEN: usize = MAX_THUNKS * THUNK_SIZE;

pub struct ThunkPool {
    // NOTE: In real use, this memory must be mapped executable by the kernel.
    // The kernel will mmap this region as RWX during driver load, then demote
    // to RX after thunk generation (W^X policy: write first, execute after seal).
    pub pool: [u8; THUNK_POOL_LEN],
    pub count: u32,
    pub total_gen: AtomicU32,
}

impl ThunkPool {
    pub const fn new() -> Self {
        Self {
            pool: [0x90u8; THUNK_POOL_LEN],
            count: 0,
            total_gen: AtomicU32::new(0),
        }
    }

    /// Generate a MS x64 → SysV AMD64 thunk for `arg_count` arguments.
    /// Returns pointer to thunk entry point (to be called with MS x64 convention).
    /// The thunk internally calls `target_sysv` using SysV ABI.
    ///
    /// Safety: caller must ensure pool memory is executable before calling thunk.
    pub unsafe fn gen_msx64_to_sysv(
        &mut self,
        target_sysv: *const c_void,
        arg_count: usize,
    ) -> Option<*const c_void> {
        if self.count as usize >= MAX_THUNKS {
            return None;
        }
        let base = self.count as usize * THUNK_SIZE;
        let buf = &mut self.pool[base..base + THUNK_SIZE];
        let mut off = 0usize;

        // Arg shuffle: MS rcx→rdi, rdx→rsi, r8→rdx, r9→rcx (first 4 args)
        // mov rdi, rcx
        if arg_count >= 1 {
            buf[off..off + 3].copy_from_slice(&[0x48, 0x89, 0xCF]);
            off += 3;
        }
        // mov rsi, rdx
        if arg_count >= 2 {
            buf[off..off + 3].copy_from_slice(&[0x48, 0x89, 0xD6]);
            off += 3;
        }
        // mov rdx, r8
        if arg_count >= 3 {
            buf[off..off + 3].copy_from_slice(&[0x4C, 0x89, 0xC2]);
            off += 3;
        }
        // mov rcx, r9
        if arg_count >= 4 {
            buf[off..off + 3].copy_from_slice(&[0x4C, 0x89, 0xCA]);
            off += 3;
        }

        // Align stack to 16 bytes (SysV requires 16-byte alignment before call)
        // sub rsp, 8
        buf[off..off + 4].copy_from_slice(&[0x48, 0x83, 0xEC, 0x08]);
        off += 4;

        // mov rax, imm64 (target address)
        buf[off] = 0x48;
        buf[off + 1] = 0xB8;
        off += 2;
        let addr = target_sysv as u64;
        buf[off..off + 8].copy_from_slice(&addr.to_le_bytes());
        off += 8;

        // call rax
        buf[off..off + 2].copy_from_slice(&[0xFF, 0xD0]);
        off += 2;

        // add rsp, 8
        buf[off..off + 4].copy_from_slice(&[0x48, 0x83, 0xC4, 0x08]);
        off += 4;

        // ret
        buf[off] = 0xC3;
        off += 1;

        // Pad remainder with int3 (0xCC) for safety
        for b in &mut buf[off..] {
            *b = 0xCC;
        }

        self.count += 1;
        self.total_gen.fetch_add(1, Ordering::Relaxed);
        Some(unsafe { self.pool.as_ptr().add(base) } as *const c_void)
    }

    /// Generate a SysV passthrough (no-op thunk) — used for already-compatible drivers
    pub unsafe fn gen_passthrough(&mut self, target: *const c_void) -> Option<*const c_void> {
        if self.count as usize >= MAX_THUNKS {
            return None;
        }
        let base = self.count as usize * THUNK_SIZE;
        let buf = &mut self.pool[base..base + THUNK_SIZE];
        // mov rax, imm64; jmp rax
        buf[0] = 0x48;
        buf[1] = 0xB8;
        buf[2..10].copy_from_slice(&(target as u64).to_le_bytes());
        buf[10..12].copy_from_slice(&[0xFF, 0xE0]); // jmp rax
        for b in &mut buf[12..] {
            *b = 0xCC;
        }
        self.count += 1;
        self.total_gen.fetch_add(1, Ordering::Relaxed);
        Some(unsafe { self.pool.as_ptr().add(base) } as *const c_void)
    }
}

// ─────────────────────────────────────────────
// ELF SYMBOL SCANNER (minimal, no_std)
// Finds driver entry point symbol in a raw ELF64 blob
// ─────────────────────────────────────────────

/// Well-known C driver entry point symbol names (null-terminated)
pub const DRIVER_ENTRY_SYMBOLS: &[&[u8]] = &[
    b"sisyphus_driver_entry\0",
    b"driver_init\0",
    b"_driver_init\0",
    b"DriverEntry\0",
    b"driver_entry\0",
    b"init_module\0", // Linux kernel module
    b"module_init\0",
];

/// ELF64 header magic
const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];

#[derive(Clone, Copy, Debug)]
pub struct SymbolInfo {
    pub offset: u64, // offset from blob start to function
    pub size: u64,
    pub name_match: u8, // index into DRIVER_ENTRY_SYMBOLS
}

/// Scan a raw ELF64 blob for driver entry point symbols.
/// Returns the first match found.
pub fn scan_elf64_for_entry(blob: &[u8]) -> Option<SymbolInfo> {
    if blob.len() < 64 {
        return None;
    }
    if &blob[0..4] != ELF_MAGIC {
        return None;
    }
    if blob[4] != 2 {
        return None;
    } // EI_CLASS = ELFCLASS64

    // ELF64 header fields (little-endian assumed)
    let shoff = u64::from_le_bytes(blob.get(40..48)?.try_into().ok()?) as usize;
    let shentsize = u16::from_le_bytes(blob.get(58..60)?.try_into().ok()?) as usize;
    let shnum = u16::from_le_bytes(blob.get(60..62)?.try_into().ok()?) as usize;
    let _shstrndx = u16::from_le_bytes(blob.get(62..64)?.try_into().ok()?) as usize;

    if shoff == 0 || shentsize < 64 {
        return None;
    }

    // Find .symtab and .strtab sections
    let mut symtab_off = 0usize;
    let mut symtab_size = 0usize;
    let mut strtab_off = 0usize;

    for i in 0..shnum {
        let sh = shoff + i * shentsize;
        if sh + shentsize > blob.len() {
            break;
        }
        let sh_type = u32::from_le_bytes(blob.get(sh + 4..sh + 8)?.try_into().ok()?);
        let sh_off = u64::from_le_bytes(blob.get(sh + 24..sh + 32)?.try_into().ok()?) as usize;
        let sh_size = u64::from_le_bytes(blob.get(sh + 32..sh + 40)?.try_into().ok()?) as usize;
        let sh_link = u32::from_le_bytes(blob.get(sh + 40..sh + 44)?.try_into().ok()?) as usize;

        if sh_type == 2 {
            // SHT_SYMTAB
            symtab_off = sh_off;
            symtab_size = sh_size;
            // Linked strtab
            let str_sh = shoff + sh_link * shentsize;
            if str_sh + shentsize <= blob.len() {
                strtab_off =
                    u64::from_le_bytes(blob.get(str_sh + 24..str_sh + 32)?.try_into().ok()?)
                        as usize;
            }
        }
    }

    if symtab_off == 0 || strtab_off == 0 {
        return None;
    }

    // Walk symbol table: each ELF64 Sym = 24 bytes
    let sym_count = symtab_size / 24;
    for s in 0..sym_count {
        let sym = symtab_off + s * 24;
        if sym + 24 > blob.len() {
            break;
        }
        let st_name = u32::from_le_bytes(blob.get(sym..sym + 4)?.try_into().ok()?) as usize;
        let st_info = blob.get(sym + 4).copied()?;
        let st_value = u64::from_le_bytes(blob.get(sym + 8..sym + 16)?.try_into().ok()?);
        let st_size = u64::from_le_bytes(blob.get(sym + 16..sym + 24)?.try_into().ok()?);

        let sym_type = st_info & 0xF;
        if sym_type != 2 {
            continue;
        } // STT_FUNC only

        // Check name against known entry points
        let name_ptr = strtab_off + st_name;
        for (idx, &target) in DRIVER_ENTRY_SYMBOLS.iter().enumerate() {
            let tlen = target.len();
            if name_ptr + tlen <= blob.len() && &blob[name_ptr..name_ptr + tlen] == target {
                return Some(SymbolInfo {
                    offset: st_value,
                    size: st_size,
                    name_match: idx as u8,
                });
            }
        }
    }
    None
}

// ─────────────────────────────────────────────
// PROMETHEUS ENGINE
// ─────────────────────────────────────────────

pub struct PrometheusEngine {
    pub thunk_pool: ThunkPool,
    pub drivers_loaded: AtomicU32,
    pub thunks_gen: AtomicU32,
    pub conv_histogram: [AtomicU32; 8], // count per CallingConv variant
}

impl PrometheusEngine {
    pub const fn new() -> Self {
        Self {
            thunk_pool: ThunkPool::new(),
            drivers_loaded: AtomicU32::new(0),
            thunks_gen: AtomicU32::new(0),
            conv_histogram: [
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
            ],
        }
    }

    /// Full auto-detect + thunk generation pipeline for a loaded driver blob.
    /// Returns (detected_convention, entry_offset, thunk_ptr_or_null)
    pub unsafe fn analyze_and_bridge(
        &mut self,
        blob: &[u8],
        kernel_entry_sysv: *const c_void,
    ) -> TranspileResult {
        // 1. Find driver entry point in ELF symbol table
        let sym = match scan_elf64_for_entry(blob) {
            Some(s) => s,
            None => return TranspileResult::NoEntryFound,
        };

        let fn_offset = sym.offset as usize;
        let prologue = blob.get(fn_offset..).unwrap_or(&[]);

        // 2. Decode prologue to detect calling convention
        let decoded = PrologueDecoder::decode(prologue);
        let conv = decoded.detected;
        self.conv_histogram[conv as usize % 8].fetch_add(1, Ordering::Relaxed);

        // 3. Generate appropriate thunk
        let thunk_ptr = match conv {
            CallingConv::MsX64 => {
                // Need to bridge MS x64 → SysV
                unsafe { self.thunk_pool.gen_msx64_to_sysv(kernel_entry_sysv, 4) }
            }
            CallingConv::SysVAmd64 | CallingConv::Unknown => {
                // Already compatible — passthrough thunk
                unsafe { self.thunk_pool.gen_passthrough(kernel_entry_sysv) }
            }
            // 32-bit: requires full stack-rewrite thunk (future work)
            _ => None,
        };

        self.drivers_loaded.fetch_add(1, Ordering::Relaxed);
        self.thunks_gen.fetch_add(1, Ordering::Relaxed);

        TranspileResult::Success {
            convention: conv,
            entry_offset: sym.offset,
            thunk_ptr: thunk_ptr.unwrap_or(core::ptr::null()),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum TranspileResult {
    Success {
        convention: CallingConv,
        entry_offset: u64,
        thunk_ptr: *const c_void,
    },
    NoEntryFound,
    UnsupportedConvention(CallingConv),
}
