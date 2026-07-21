use crate::module::relocator::ExternalSymbolResolver;
use crate::shim::linux_kpi;

const MAXIMUM_SYMBOLS: usize = 64;

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
            OsPersonality::WindowsNt(_) => {
                return Err(PersonalityError::UnavailablePersonality);
            }
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

    fn materialize_linux(&mut self, _version: LinuxVersion) -> Result<(), PersonalityError> {
        let allocate = linux_kpi::kmalloc as *const () as usize as u64;
        let deallocate = linux_kpi::kfree as *const () as usize as u64;
        let log = linux_kpi::printk as *const () as usize as u64;
        self.virtual_vtable = EnvironmentVTable {
            allocate: Some(allocate),
            deallocate: Some(deallocate),
            log: Some(log),
            device_control: None,
        };
        self.insert_symbol(b"kmalloc", allocate)?;
        self.insert_symbol(b"__kmalloc", allocate)?;
        self.insert_symbol(b"kfree", deallocate)?;
        self.insert_symbol(b"printk", log)?;
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
    InvalidSymbol,
    DuplicateSymbol,
    SymbolCapacityExceeded,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materializes_the_versioned_linux_symbol_subset() {
        let enclave = MirageEnclave::materialize(OsPersonality::Linux(LinuxVersion::V6_1)).unwrap();

        assert_eq!(enclave.object_format(), ObjectFormat::ElfRelocatable);
        assert_eq!(enclave.calling_convention(), CallingConvention::SystemV64);
        assert_eq!(
            enclave.compatibility_level(),
            CompatibilityLevel::SymbolSubset
        );
        assert_eq!(enclave.resolve(b"kmalloc"), enclave.resolve(b"__kmalloc"));
        assert!(enclave.resolve(b"schedule_work").is_none());
        assert_eq!(enclave.symbols().len(), 4);
    }

    #[test]
    fn refuses_unimplemented_personalities() {
        assert!(matches!(
            MirageEnclave::materialize(OsPersonality::WindowsNt(NtVersion::Windows11)),
            Err(PersonalityError::UnavailablePersonality)
        ));
        assert!(matches!(
            MirageEnclave::materialize(OsPersonality::FreeBsd(BsdVersion::V14_1)),
            Err(PersonalityError::UnavailablePersonality)
        ));
    }
}
