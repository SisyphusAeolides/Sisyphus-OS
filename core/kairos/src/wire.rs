use crate::profile::{MAXIMUM_CPUS, MAXIMUM_IO_DEVICES};
use crate::topology::MAXIMUM_DOMAINS;

pub const MAXIMUM_CPUS_PER_DOMAIN: usize = MAXIMUM_CPUS;

pub mod features {
    pub const SYSCALL_BASIC: u64 = 1 << 0;
    pub const SYSCALL_EXTENDED: u64 = 1 << 1;
    pub const SYSCALL_DRIVER: u64 = 1 << 2;
    pub const ASYNC_IO: u64 = 1 << 3;
    pub const DMA_RING: u64 = 1 << 4;
    pub const SHARED_MEMORY: u64 = 1 << 5;
    pub const CAPABILITY_IPC: u64 = 1 << 6;
    pub const THERMAL_PAGE: u64 = 1 << 7;
    pub const KAIROS_PAGE: u64 = 1 << 8;
    pub const TACHYON_YIELD: u64 = 1 << 9;
    pub const HOLOGRAM_FS: u64 = 1 << 10;
    pub const OFFLOAD_DISPATCH: u64 = 1 << 11;
}

pub mod trait_flags {
    pub const SMP: u32 = 1 << 0;
    pub const NUMA: u32 = 1 << 1;
    pub const HETEROGENEOUS: u32 = 1 << 2;
    pub const OFFLOAD: u32 = 1 << 3;
    pub const PERSISTENT_MEM: u32 = 1 << 4;
    pub const SHARED_MEM: u32 = 1 << 5;
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct RawCpuEntry {
    pub logical_id: u16,
    pub _pad0: u16,
    pub hardware_id: u32,
    pub package: u16,
    pub core: u16,
    pub thread: u16,
    pub numa_domain: u16,
    pub kind: u8,
    pub enabled: u8,
    pub _pad1: [u8; 2],
}

impl RawCpuEntry {
    pub const ZERO: Self = Self {
        logical_id: 0,
        _pad0: 0,
        hardware_id: 0,
        package: 0,
        core: 0,
        thread: 0,
        numa_domain: 0,
        kind: 0,
        enabled: 0,
        _pad1: [0; 2],
    };
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct RawDomainEntry {
    pub id: u16,
    pub kind: u8,
    pub parent_valid: u8,
    pub parent_id: u16,
    pub member_count: u16,
    pub members: [u16; MAXIMUM_CPUS_PER_DOMAIN],
}

impl RawDomainEntry {
    pub const ZERO: Self = Self {
        id: 0,
        kind: 0,
        parent_valid: 0,
        parent_id: 0,
        member_count: 0,
        members: [0; MAXIMUM_CPUS_PER_DOMAIN],
    };
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct RawTopologyHeader {
    pub cpu_count: u32,
    pub domain_count: u32,
    pub trait_flags: u32,
    pub _pad: u32,
}

impl RawTopologyHeader {
    pub const ZERO: Self = Self {
        cpu_count: 0,
        domain_count: 0,
        trait_flags: 0,
        _pad: 0,
    };
}

#[repr(C)]
pub struct RawTopologyReply {
    pub header: RawTopologyHeader,
    pub cpus: [RawCpuEntry; MAXIMUM_CPUS],
    pub domains: [RawDomainEntry; MAXIMUM_DOMAINS],
}

impl RawTopologyReply {
    pub const fn zeroed() -> Self {
        Self {
            header: RawTopologyHeader::ZERO,
            cpus: [RawCpuEntry::ZERO; MAXIMUM_CPUS],
            domains: [RawDomainEntry::ZERO; MAXIMUM_DOMAINS],
        }
    }
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct AbiRequest {
    pub magic: u32,
    pub version: u16,
    pub structure_size: u16,
    pub endian: u8,
    pub word_bits: u8,
    pub pointer_bits: u8,
    pub abi_kind: u8,
    pub page_size: u32,
    pub syscall_style: u16,
    pub object_bits: u16,
    pub _pad: u32,
    pub features_lo_req: u64,
    pub features_hi_req: u64,
    pub features_lo_opt: u64,
    pub features_hi_opt: u64,
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct AbiReply {
    pub features_lo_granted: u64,
    pub features_hi_granted: u64,
    pub features_lo_unavailable: u64,
    pub features_hi_unavailable: u64,
    pub status: u32,
    pub _pad: u32,
}

impl AbiReply {
    pub const ZERO: Self = Self {
        features_lo_granted: 0,
        features_hi_granted: 0,
        features_lo_unavailable: 0,
        features_hi_unavailable: 0,
        status: 0,
        _pad: 0,
    };
}

const _: () = {
    assert!(MAXIMUM_CPUS == 256);
    assert!(MAXIMUM_DOMAINS == 128);
    assert!(MAXIMUM_IO_DEVICES == 256);
    assert!(core::mem::size_of::<RawCpuEntry>() == 20);
    assert!(core::mem::size_of::<RawDomainEntry>() == 520);
    assert!(core::mem::size_of::<RawTopologyHeader>() == 16);
    assert!(core::mem::size_of::<RawTopologyReply>() == 71_696);
    assert!(core::mem::size_of::<AbiRequest>() == 56);
    assert!(core::mem::size_of::<AbiReply>() == 40);
};
