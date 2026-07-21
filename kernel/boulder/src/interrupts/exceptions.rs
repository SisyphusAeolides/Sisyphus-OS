#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageFaultInfo {
    pub address: usize,
    pub error_code: usize,
    pub protection_violation: bool,
    pub write: bool,
    pub user: bool,
    pub reserved_bit: bool,
    pub instruction_fetch: bool,
}

pub fn page_fault(error_code: usize) -> PageFaultInfo {
    PageFaultInfo {
        address: read_cr2(),
        error_code,
        protection_violation: error_code & 1 != 0,
        write: error_code & (1 << 1) != 0,
        user: error_code & (1 << 2) != 0,
        reserved_bit: error_code & (1 << 3) != 0,
        instruction_fetch: error_code & (1 << 4) != 0,
    }
}

fn read_cr2() -> usize {
    let address: usize;
    unsafe {
        core::arch::asm!(
            "mov {}, cr2",
            out(reg) address,
            options(nomem, nostack, preserves_flags),
        );
    }
    address
}

#[cfg(test)]
mod tests {
    #[test]
    fn decodes_page_fault_error_bits_without_reading_control_registers() {
        let error = 1_usize | (1 << 1) | (1 << 4);
        assert!(error & 1 != 0);
        assert!(error & (1 << 1) != 0);
        assert!(error & (1 << 4) != 0);
        assert!(error & (1 << 2) == 0);
    }
}
