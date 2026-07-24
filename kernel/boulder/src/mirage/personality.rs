use crate::mirage::ntoskrnl::abi;
use crate::module::relocator::ExternalSymbolResolver;
use crate::shim::linux_kpi;

const MAXIMUM_SYMBOLS: usize = 64;
// This is the entire version-labelled Linux contract, with printk restricted
// to the literal-only behavior documented by linux_kpi.
const LINUX_6_1_KPI_SYMBOLS: [&[u8]; 22] = [
    b"kmalloc",
    b"__kmalloc",
    b"kfree",
    b"printk",
    b"krealloc",
    b"ksize",
    b"kmemdup",
    b"kmemdup_nul",
    b"kfree_sensitive",
    b"memcpy",
    b"memmove",
    b"memset",
    b"memcmp",
    b"memchr",
    b"strlen",
    b"strnlen",
    b"strcmp",
    b"strncmp",
    b"strscpy",
    b"memzero_explicit",
    b"kstrdup",
    b"kstrndup",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OsPersonality {
    Linux(LinuxVersion),
    WindowsNt(NtVersion),
    FreeBsd(BsdVersion),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinuxVersion {
    V5_15,
    V6_1,
    V6_6,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NtVersion {
    Windows10,
    Windows11,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BsdVersion {
    V13_2,
    V14_0,
    V14_1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectFormat {
    ElfRelocatable,
    PortableExecutable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallingConvention {
    SystemV64,
    Windows64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompatibilityLevel {
    SymbolSubset,
    Complete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnvironmentVTable {
    pub allocate: Option<u64>,
    pub deallocate: Option<u64>,
    pub log: Option<u64>,
    pub device_control: Option<u64>,
}

impl EnvironmentVTable {
    const EMPTY: Self = Self {
        allocate: None,
        deallocate: None,
        log: None,
        device_control: None,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SymbolBinding {
    pub name: &'static [u8],
    pub address: u64,
}

impl SymbolBinding {
    const EMPTY: Self = Self {
        name: &[],
        address: 0,
    };
}

pub struct MirageEnclave {
    personality: OsPersonality,
    object_format: ObjectFormat,
    calling_convention: CallingConvention,
    compatibility_level: CompatibilityLevel,
    virtual_vtable: EnvironmentVTable,
    symbols: [SymbolBinding; MAXIMUM_SYMBOLS],
    symbol_count: usize,
}

impl MirageEnclave {
    pub fn materialize(personality: OsPersonality) -> Result<Self, PersonalityError> {
        let mut enclave = Self {
            personality,
            object_format: ObjectFormat::ElfRelocatable,
            calling_convention: CallingConvention::SystemV64,
            compatibility_level: CompatibilityLevel::SymbolSubset,
            virtual_vtable: EnvironmentVTable::EMPTY,
            symbols: [SymbolBinding::EMPTY; MAXIMUM_SYMBOLS],
            symbol_count: 0,
        };

        match personality {
            OsPersonality::Linux(version) => enclave.materialize_linux(version)?,
            OsPersonality::WindowsNt(version) => enclave.materialize_windows(version)?,
            OsPersonality::FreeBsd(_) => {
                return Err(PersonalityError::UnavailablePersonality);
            }
        }
        Ok(enclave)
    }

    pub const fn personality(&self) -> OsPersonality {
        self.personality
    }

    pub const fn object_format(&self) -> ObjectFormat {
        self.object_format
    }

    pub const fn calling_convention(&self) -> CallingConvention {
        self.calling_convention
    }

    pub const fn compatibility_level(&self) -> CompatibilityLevel {
        self.compatibility_level
    }

    pub const fn virtual_vtable(&self) -> EnvironmentVTable {
        self.virtual_vtable
    }

    pub fn symbols(&self) -> &[SymbolBinding] {
        &self.symbols[..self.symbol_count]
    }

    fn materialize_linux(&mut self, version: LinuxVersion) -> Result<(), PersonalityError> {
        if version != LinuxVersion::V6_1 {
            return Err(PersonalityError::UnsupportedVersion);
        }
        if !linux_kpi::is_ready() {
            return Err(PersonalityError::RuntimeUnavailable);
        }
        let allocate = linux_kpi::kmalloc as *const () as usize as u64;
        let deallocate = linux_kpi::kfree as *const () as usize as u64;
        let log = linux_kpi::printk as *const () as usize as u64;
        self.virtual_vtable = EnvironmentVTable {
            allocate: Some(allocate),
            deallocate: Some(deallocate),
            log: Some(log),
            device_control: None,
        };
        let addresses = [
            allocate,
            allocate,
            deallocate,
            log,
            linux_kpi::krealloc as *const () as usize as u64,
            linux_kpi::ksize as *const () as usize as u64,
            linux_kpi::kmemdup as *const () as usize as u64,
            linux_kpi::kmemdup_nul as *const () as usize as u64,
            linux_kpi::kfree_sensitive as *const () as usize as u64,
            linux_kpi::linux_memcpy as *const () as usize as u64,
            linux_kpi::linux_memmove as *const () as usize as u64,
            linux_kpi::linux_memset as *const () as usize as u64,
            linux_kpi::linux_memcmp as *const () as usize as u64,
            linux_kpi::linux_memchr as *const () as usize as u64,
            linux_kpi::linux_strlen as *const () as usize as u64,
            linux_kpi::linux_strnlen as *const () as usize as u64,
            linux_kpi::linux_strcmp as *const () as usize as u64,
            linux_kpi::linux_strncmp as *const () as usize as u64,
            linux_kpi::linux_strscpy as *const () as usize as u64,
            linux_kpi::linux_memzero_explicit as *const () as usize as u64,
            linux_kpi::kstrdup as *const () as usize as u64,
            linux_kpi::kstrndup as *const () as usize as u64,
        ];
        for (name, address) in LINUX_6_1_KPI_SYMBOLS.into_iter().zip(addresses) {
            self.insert_symbol(name, address)?;
        }
        Ok(())
    }

    fn materialize_windows(&mut self, _version: NtVersion) -> Result<(), PersonalityError> {
        let allocate = abi::ex_allocate_pool_with_tag as *const () as usize as u64;
        let deallocate = abi::ex_free_pool_with_tag as *const () as usize as u64;
        self.object_format = ObjectFormat::PortableExecutable;
        self.calling_convention = CallingConvention::Windows64;
        self.virtual_vtable = EnvironmentVTable {
            allocate: Some(allocate),
            deallocate: Some(deallocate),
            log: None,
            device_control: None,
        };
        self.insert_symbol(b"ExAllocatePoolWithTag", allocate)?;
        self.insert_symbol(b"ExFreePoolWithTag", deallocate)?;
        Ok(())
    }

    fn insert_symbol(&mut self, name: &'static [u8], address: u64) -> Result<(), PersonalityError> {
        if name.is_empty() || address == 0 {
            return Err(PersonalityError::InvalidSymbol);
        }
        if self.symbols().iter().any(|binding| binding.name == name) {
            return Err(PersonalityError::DuplicateSymbol);
        }
        let slot = self
            .symbols
            .get_mut(self.symbol_count)
            .ok_or(PersonalityError::SymbolCapacityExceeded)?;
        *slot = SymbolBinding { name, address };
        self.symbol_count += 1;
        Ok(())
    }
}

impl ExternalSymbolResolver for MirageEnclave {
    fn resolve(&self, name: &[u8]) -> Option<u64> {
        self.symbols()
            .iter()
            .find(|binding| binding.name == name)
            .map(|binding| binding.address)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersonalityError {
    UnavailablePersonality,
    UnsupportedVersion,
    RuntimeUnavailable,
    InvalidSymbol,
    DuplicateSymbol,
    SymbolCapacityExceeded,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct InstalledApi;

    impl Drop for InstalledApi {
        fn drop(&mut self) {
            let _ = unsafe { linux_kpi::uninstall() };
        }
    }

    #[test]
    fn linux_6_1_materialization_requires_the_exact_live_subset() {
        let _lock = linux_kpi::TEST_INSTALL_LOCK.lock();
        let _ = unsafe { linux_kpi::uninstall() };
        assert!(matches!(
            MirageEnclave::materialize(OsPersonality::Linux(LinuxVersion::V6_1)),
            Err(PersonalityError::RuntimeUnavailable)
        ));

        assert_eq!(
            unsafe { linux_kpi::install(&linux_kpi::TEST_KERNEL_API) },
            Ok(())
        );
        let _installed = InstalledApi;
        let enclave = MirageEnclave::materialize(OsPersonality::Linux(LinuxVersion::V6_1)).unwrap();

        assert_eq!(enclave.object_format(), ObjectFormat::ElfRelocatable);
        assert_eq!(enclave.calling_convention(), CallingConvention::SystemV64);
        assert_eq!(
            enclave.compatibility_level(),
            CompatibilityLevel::SymbolSubset
        );
        assert_eq!(enclave.resolve(b"kmalloc"), enclave.resolve(b"__kmalloc"));
        assert_eq!(
            enclave.resolve(b"strscpy"),
            Some(linux_kpi::linux_strscpy as *const () as usize as u64)
        );
        assert_eq!(
            enclave.resolve(b"kstrndup"),
            Some(linux_kpi::kstrndup as *const () as usize as u64)
        );
        assert!(enclave.resolve(b"schedule_work").is_none());
        assert_eq!(enclave.symbols().len(), LINUX_6_1_KPI_SYMBOLS.len());
        for (binding, expected_name) in enclave.symbols().iter().zip(LINUX_6_1_KPI_SYMBOLS) {
            assert_eq!(binding.name, expected_name);
        }

        let _ = unsafe { linux_kpi::uninstall() };
        assert!(matches!(
            MirageEnclave::materialize(OsPersonality::Linux(LinuxVersion::V6_1)),
            Err(PersonalityError::RuntimeUnavailable)
        ));
    }

    #[test]
    fn rejects_linux_versions_without_an_explicit_subset_contract() {
        let _lock = linux_kpi::TEST_INSTALL_LOCK.lock();
        let _ = unsafe { linux_kpi::uninstall() };
        assert_eq!(
            unsafe { linux_kpi::install(&linux_kpi::TEST_KERNEL_API) },
            Ok(())
        );
        let _installed = InstalledApi;

        for version in [LinuxVersion::V5_15, LinuxVersion::V6_6] {
            assert!(matches!(
                MirageEnclave::materialize(OsPersonality::Linux(version)),
                Err(PersonalityError::UnsupportedVersion)
            ));
        }
    }

    #[test]
    fn materializes_only_the_implemented_windows_subset() {
        let enclave =
            MirageEnclave::materialize(OsPersonality::WindowsNt(NtVersion::Windows11)).unwrap();
        assert_eq!(enclave.object_format(), ObjectFormat::PortableExecutable);
        assert_eq!(enclave.calling_convention(), CallingConvention::Windows64);
        assert!(enclave.resolve(b"ExAllocatePoolWithTag").is_some());
        assert!(enclave.resolve(b"KeInitializeEvent").is_none());
    }

    #[test]
    fn refuses_unimplemented_freebsd_personalities() {
        assert!(matches!(
            MirageEnclave::materialize(OsPersonality::FreeBsd(BsdVersion::V14_1)),
            Err(PersonalityError::UnavailablePersonality)
        ));
    }
}
