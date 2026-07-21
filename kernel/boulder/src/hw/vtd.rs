use core::sync::atomic::{AtomicU64, Ordering};

use sisyphus_driver_abi::{STATUS_INVALID_ARGUMENT, STATUS_OK, Status};

use super::pci::PciAddress;

const PAGE_SIZE: u64 = 4096;
const PHYSICAL_PAGE_MASK: u64 = 0x000f_ffff_ffff_f000;
const PRESENT: u64 = 1;

#[repr(C, align(16))]
pub struct RootEntry {
    lower: AtomicU64,
    upper: AtomicU64,
}

impl RootEntry {
    pub const fn new() -> Self {
        Self {
            lower: AtomicU64::new(0),
            upper: AtomicU64::new(0),
        }
    }

    pub fn install_context_table(&self, physical_address: u64) -> Result<(), TableError> {
        validate_page(physical_address)?;
        self.upper.store(0, Ordering::Relaxed);
        self.lower.store(
            (physical_address & PHYSICAL_PAGE_MASK) | PRESENT,
            Ordering::Release,
        );
        Ok(())
    }

    pub fn clear(&self) {
        self.lower.store(0, Ordering::Release);
        self.upper.store(0, Ordering::Relaxed);
    }

    pub fn raw(&self) -> (u64, u64) {
        (
            self.lower.load(Ordering::Acquire),
            self.upper.load(Ordering::Acquire),
        )
    }
}

impl Default for RootEntry {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(C, align(4096))]
pub struct RootEntryTable {
    entries: [RootEntry; 256],
}

impl RootEntryTable {
    pub const fn new() -> Self {
        Self {
            entries: [const { RootEntry::new() }; 256],
        }
    }

    pub fn entry(&self, bus: u8) -> &RootEntry {
        &self.entries[bus as usize]
    }
}

impl Default for RootEntryTable {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(C, align(16))]
pub struct ContextEntry {
    lower: AtomicU64,
    upper: AtomicU64,
}

impl ContextEntry {
    pub const fn new() -> Self {
        Self {
            lower: AtomicU64::new(0),
            upper: AtomicU64::new(0),
        }
    }

    pub fn install_second_level_translation(
        &self,
        domain_id: u16,
        address_width: u8,
        page_table_root: u64,
    ) -> Result<(), TableError> {
        validate_page(page_table_root)?;
        if address_width > 4 {
            return Err(TableError::InvalidAddressWidth);
        }
        let upper = (u64::from(domain_id) << 8) | u64::from(address_width);
        self.upper.store(upper, Ordering::Relaxed);
        self.lower.store(
            (page_table_root & PHYSICAL_PAGE_MASK) | PRESENT,
            Ordering::Release,
        );
        Ok(())
    }

    pub fn clear(&self) {
        self.lower.store(0, Ordering::Release);
        self.upper.store(0, Ordering::Relaxed);
    }

    pub fn raw(&self) -> (u64, u64) {
        (
            self.lower.load(Ordering::Acquire),
            self.upper.load(Ordering::Acquire),
        )
    }
}

impl Default for ContextEntry {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(C, align(4096))]
pub struct ContextEntryTable {
    entries: [ContextEntry; 256],
}

impl ContextEntryTable {
    pub const fn new() -> Self {
        Self {
            entries: [const { ContextEntry::new() }; 256],
        }
    }

    pub fn entry(&self, device: PciAddress) -> &ContextEntry {
        &self.entries[device.slot as usize * 8 + device.function as usize]
    }
}

impl Default for ContextEntryTable {
    fn default() -> Self {
        Self::new()
    }
}

pub trait VtdHardware: Sync {
    fn install_root_table(&self, physical_address: u64) -> Status;
    fn invalidate_context_cache(&self, domain_id: u16) -> Status;
    fn invalidate_iotlb(&self, domain_id: u16) -> Status;
    fn enable_translation(&self) -> Status;
}

pub struct DeviceContext<'a> {
    pub root_table: &'a RootEntryTable,
    pub root_table_physical_address: u64,
    pub context_table: &'a ContextEntryTable,
    pub context_table_physical_address: u64,
    pub device: PciAddress,
    pub domain_id: u16,
    pub address_width: u8,
    pub page_table_root: u64,
}

pub fn activate_device_context(context: &DeviceContext<'_>, hardware: &dyn VtdHardware) -> Status {
    if context.device.bus as usize >= 256
        || validate_page(context.root_table_physical_address).is_err()
        || validate_page(context.context_table_physical_address).is_err()
        || validate_page(context.page_table_root).is_err()
    {
        return STATUS_INVALID_ARGUMENT;
    }
    if context
        .context_table
        .entry(context.device)
        .install_second_level_translation(
            context.domain_id,
            context.address_width,
            context.page_table_root,
        )
        .is_err()
        || context
            .root_table
            .entry(context.device.bus)
            .install_context_table(context.context_table_physical_address)
            .is_err()
    {
        return STATUS_INVALID_ARGUMENT;
    }

    let status = hardware.install_root_table(context.root_table_physical_address);
    if status != STATUS_OK {
        return status;
    }
    let status = hardware.invalidate_context_cache(context.domain_id);
    if status != STATUS_OK {
        return status;
    }
    let status = hardware.invalidate_iotlb(context.domain_id);
    if status != STATUS_OK {
        return status;
    }
    hardware.enable_translation()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TableError {
    InvalidPageAddress,
    InvalidAddressWidth,
}

fn validate_page(physical_address: u64) -> Result<(), TableError> {
    if physical_address == 0
        || physical_address % PAGE_SIZE != 0
        || physical_address & !PHYSICAL_PAGE_MASK != 0
    {
        Err(TableError::InvalidPageAddress)
    } else {
        Ok(())
    }
}

const _: () = assert!(core::mem::size_of::<RootEntry>() == 16);
const _: () = assert!(core::mem::size_of::<RootEntryTable>() == 4096);
const _: () = assert!(core::mem::size_of::<ContextEntry>() == 16);
const _: () = assert!(core::mem::size_of::<ContextEntryTable>() == 4096);

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct TestHardware {
        calls: AtomicUsize,
    }

    impl VtdHardware for TestHardware {
        fn install_root_table(&self, _physical_address: u64) -> Status {
            self.calls.fetch_add(1, Ordering::Relaxed);
            STATUS_OK
        }

        fn invalidate_context_cache(&self, _domain_id: u16) -> Status {
            self.calls.fetch_add(1, Ordering::Relaxed);
            STATUS_OK
        }

        fn invalidate_iotlb(&self, _domain_id: u16) -> Status {
            self.calls.fetch_add(1, Ordering::Relaxed);
            STATUS_OK
        }

        fn enable_translation(&self) -> Status {
            self.calls.fetch_add(1, Ordering::Relaxed);
            STATUS_OK
        }
    }

    #[test]
    fn builds_full_width_root_and_context_entries() {
        let roots = RootEntryTable::new();
        let contexts = ContextEntryTable::new();
        let hardware = TestHardware {
            calls: AtomicUsize::new(0),
        };
        let device = PciAddress::new(2, 3, 1).unwrap();
        let context = DeviceContext {
            root_table: &roots,
            root_table_physical_address: 0x1000,
            context_table: &contexts,
            context_table_physical_address: 0x2000,
            device,
            domain_id: 7,
            address_width: 2,
            page_table_root: 0x3000,
        };
        assert_eq!(activate_device_context(&context, &hardware), STATUS_OK);
        assert_eq!(roots.entry(2).raw(), (0x2001, 0));
        assert_eq!(contexts.entry(device).raw(), (0x3001, 7 << 8 | 2));
        assert_eq!(hardware.calls.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn rejects_unaligned_table_addresses() {
        let entry = RootEntry::new();
        assert_eq!(
            entry.install_context_table(0x1234),
            Err(TableError::InvalidPageAddress)
        );
    }
}
