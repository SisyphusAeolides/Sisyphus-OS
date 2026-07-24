use core::mem::size_of;

use crate::arch::x86_64::privilege::{
    self, DOUBLE_FAULT_IST_INDEX, FaultStackInfo, KERNEL_CODE_SELECTOR, MACHINE_CHECK_IST_INDEX,
    NMI_IST_INDEX, PrivilegeError,
};

const IDT_ENTRIES: usize = 256;
const INTERRUPT_STUBS: usize = 50;
const INTERRUPT_GATE: u8 = 0x8e;
const USER_INTERRUPT_GATE: u8 = 0xee;
const NMI_VECTOR: usize = 2;
const DOUBLE_FAULT_VECTOR: usize = 8;
const MACHINE_CHECK_VECTOR: usize = 18;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdtInfo {
    pub fault_stacks: FaultStackInfo,
    pub double_fault_ist: u8,
    pub non_maskable_interrupt_ist: u8,
    pub machine_check_ist: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdtError {
    Privilege(PrivilegeError),
    InvalidIstIndex,
    InvalidDescriptor,
    DescriptorLoadFailed,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    attributes: u8,
    offset_middle: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const MISSING: Self = Self {
        offset_low: 0,
        selector: 0,
        ist: 0,
        attributes: 0,
        offset_middle: 0,
        offset_high: 0,
        reserved: 0,
    };

    const fn interrupt_gate(
        handler: usize,
        attributes: u8,
        ist_index: u8,
    ) -> Result<Self, IdtError> {
        if ist_index > 7 {
            return Err(IdtError::InvalidIstIndex);
        }
        Ok(Self {
            offset_low: handler as u16,
            selector: KERNEL_CODE_SELECTOR,
            ist: ist_index,
            attributes,
            offset_middle: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            reserved: 0,
        })
    }

    const fn handler_address(self) -> u64 {
        self.offset_low as u64
            | ((self.offset_middle as u64) << 16)
            | ((self.offset_high as u64) << 32)
    }
}

#[repr(C, packed)]
struct IdtPointer {
    limit: u16,
    base: u64,
}

unsafe extern "C" {
    static isr_stub_table: [usize; INTERRUPT_STUBS];
    fn isr_unhandled();
}

static mut IDT: [IdtEntry; IDT_ENTRIES] = [IdtEntry::MISSING; IDT_ENTRIES];

/// Installs Boulder's exception and legacy IRQ descriptor table.
///
/// # Safety
///
/// This must run once on the bootstrap CPU while interrupts are disabled. The
/// code selector and assembly stubs must match the active GDT and stack format.
pub unsafe fn initialize() -> Result<IdtInfo, IdtError> {
    let fault_stacks = privilege::validate_ist_bindings().map_err(IdtError::Privilege)?;
    let idt = core::ptr::addr_of_mut!(IDT).cast::<IdtEntry>();
    let fallback =
        IdtEntry::interrupt_gate(isr_unhandled as *const () as usize, INTERRUPT_GATE, 0)?;
    for index in 0..IDT_ENTRIES {
        unsafe { idt.add(index).write(fallback) };
    }

    let stubs = core::ptr::addr_of!(isr_stub_table).cast::<usize>();
    for index in 0..INTERRUPT_STUBS {
        let handler = unsafe { stubs.add(index).read() };
        let attributes = if index == 3 {
            USER_INTERRUPT_GATE
        } else {
            INTERRUPT_GATE
        };
        let entry = IdtEntry::interrupt_gate(handler, attributes, ist_index_for_vector(index))?;
        unsafe { idt.add(index).write(entry) };
    }

    for index in 0..IDT_ENTRIES {
        let entry = unsafe { idt.add(index).read() };
        if !valid_descriptor(index, entry) {
            return Err(IdtError::InvalidDescriptor);
        }
    }

    let pointer = IdtPointer {
        limit: (size_of::<IdtEntry>() * IDT_ENTRIES - 1) as u16,
        base: idt as u64,
    };
    unsafe {
        core::arch::asm!(
            "lidt [{}]",
            in(reg) &pointer,
            options(readonly, nostack, preserves_flags),
        );
    }
    let mut loaded = IdtPointer { limit: 0, base: 0 };
    // SAFETY: SIDT only snapshots the current CPU's descriptor-table register.
    unsafe {
        core::arch::asm!(
            "sidt [{}]",
            in(reg) &mut loaded,
            options(nostack, preserves_flags),
        );
    }
    let loaded_pointer = core::ptr::addr_of!(loaded);
    let loaded_limit = unsafe { core::ptr::addr_of!((*loaded_pointer).limit).read_unaligned() };
    let loaded_base = unsafe { core::ptr::addr_of!((*loaded_pointer).base).read_unaligned() };
    if loaded_limit != pointer.limit || loaded_base != pointer.base {
        return Err(IdtError::DescriptorLoadFailed);
    }

    Ok(IdtInfo {
        fault_stacks,
        double_fault_ist: DOUBLE_FAULT_IST_INDEX,
        non_maskable_interrupt_ist: NMI_IST_INDEX,
        machine_check_ist: MACHINE_CHECK_IST_INDEX,
    })
}

const fn ist_index_for_vector(vector: usize) -> u8 {
    match vector {
        DOUBLE_FAULT_VECTOR => DOUBLE_FAULT_IST_INDEX,
        NMI_VECTOR => NMI_IST_INDEX,
        MACHINE_CHECK_VECTOR => MACHINE_CHECK_IST_INDEX,
        _ => 0,
    }
}

const fn valid_descriptor(vector: usize, entry: IdtEntry) -> bool {
    let expected_attributes = if vector == 3 {
        USER_INTERRUPT_GATE
    } else {
        INTERRUPT_GATE
    };
    entry.selector == KERNEL_CODE_SELECTOR
        && entry.ist == ist_index_for_vector(vector)
        && entry.attributes == expected_attributes
        && entry.reserved == 0
        && entry.handler_address() != 0
}

const _: () = assert!(size_of::<IdtEntry>() == 16);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fatal_vectors_use_three_distinct_ist_slots() {
        assert_eq!(ist_index_for_vector(DOUBLE_FAULT_VECTOR), 1);
        assert_eq!(ist_index_for_vector(NMI_VECTOR), 2);
        assert_eq!(ist_index_for_vector(MACHINE_CHECK_VECTOR), 3);
        assert_eq!(ist_index_for_vector(14), 0);
        assert_eq!(ist_index_for_vector(49), 0);
    }

    #[test]
    fn gate_encoding_preserves_full_handler_and_ist() {
        let handler = 0xffff_ffff_8123_4567usize;
        let entry = IdtEntry::interrupt_gate(handler, INTERRUPT_GATE, 3).unwrap();
        assert_eq!(entry.handler_address(), handler as u64);
        assert_eq!(entry.ist, 3);
        assert!(valid_descriptor(MACHINE_CHECK_VECTOR, entry));
    }

    #[test]
    fn invalid_ist_and_cross_vector_binding_fail_closed() {
        assert_eq!(
            IdtEntry::interrupt_gate(0x1000, INTERRUPT_GATE, 8),
            Err(IdtError::InvalidIstIndex)
        );
        let double_fault =
            IdtEntry::interrupt_gate(0x1000, INTERRUPT_GATE, DOUBLE_FAULT_IST_INDEX).unwrap();
        assert!(!valid_descriptor(NMI_VECTOR, double_fault));
    }
}
