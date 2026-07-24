use alloc::vec::Vec;
use core::mem::size_of;

use crate::module::elf_headers::{RelocationEntry, SymbolEntry};

const R_X86_64_64: u32 = 1;
const R_X86_64_PC32: u32 = 2;
const R_X86_64_PLT32: u32 = 4;
const R_X86_64_GLOB_DAT: u32 = 6;
const SECTION_UNDEFINED: u16 = 0;
const SECTION_ABSOLUTE: u16 = 0xfff1;

pub trait ExternalSymbolResolver {
    /// Resolves one symbol binding for a relocation transaction.
    fn resolve(&self, name: &[u8]) -> Option<u64>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelocationError {
    InvalidSymbolIndex,
    InvalidSectionIndex,
    InvalidString,
    UnresolvedSymbol,
    PatchOutsideImage,
    ValueOutOfRange,
    PlanAllocationFailed,
    UnsupportedRelocation(u32),
}

pub struct RelocationContext<'a> {
    pub image_virtual_address: u64,
    pub target_image_offset: usize,
    pub section_addresses: &'a [u64],
    pub symbols: &'a [SymbolEntry],
    pub strings: &'a [u8],
    pub external_symbols: &'a dyn ExternalSymbolResolver,
}

pub fn apply_relocations(
    image: &mut [u8],
    relocations: &[RelocationEntry],
    context: &RelocationContext<'_>,
) -> Result<(), RelocationError> {
    // Retain the proven values so a stateful resolver is consulted only once
    // per relocation and no fallible work remains after mutation begins.
    let mut patches = Vec::new();
    patches
        .try_reserve_exact(relocations.len())
        .map_err(|_| RelocationError::PlanAllocationFailed)?;
    for relocation in relocations {
        patches.push(evaluate_relocation(image.len(), relocation, context)?);
    }

    for patch in patches {
        match patch.value {
            RelocationValue::Absolute(value) => {
                write_bytes(image, patch.offset, &value.to_le_bytes())?;
            }
            RelocationValue::Relative(value) => {
                write_bytes(image, patch.offset, &value.to_le_bytes())?;
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum RelocationValue {
    Absolute(u64),
    Relative(i32),
}

#[derive(Clone, Copy)]
struct RelocationPatch {
    offset: usize,
    value: RelocationValue,
}

fn evaluate_relocation(
    image_length: usize,
    relocation: &RelocationEntry,
    context: &RelocationContext<'_>,
) -> Result<RelocationPatch, RelocationError> {
    let symbol_index = (relocation.information >> 32) as usize;
    let relocation_type = relocation.information as u32;
    let symbol = context
        .symbols
        .get(symbol_index)
        .copied()
        .ok_or(RelocationError::InvalidSymbolIndex)?;
    let symbol_address = resolve_symbol(symbol, context)?;
    let relocation_offset = usize::try_from(relocation.offset)
        .ok()
        .and_then(|offset| context.target_image_offset.checked_add(offset))
        .ok_or(RelocationError::PatchOutsideImage)?;
    let place = context
        .image_virtual_address
        .checked_add(relocation_offset as u64)
        .ok_or(RelocationError::ValueOutOfRange)?;
    let value = i128::from(symbol_address) + i128::from(relocation.addend);

    let value = match relocation_type {
        R_X86_64_64 | R_X86_64_GLOB_DAT => {
            let value = u64::try_from(value).map_err(|_| RelocationError::ValueOutOfRange)?;
            validate_patch_range(image_length, relocation_offset, size_of::<u64>())?;
            RelocationValue::Absolute(value)
        }
        R_X86_64_PC32 | R_X86_64_PLT32 => {
            let relative = value - i128::from(place);
            let relative = i32::try_from(relative).map_err(|_| RelocationError::ValueOutOfRange)?;
            validate_patch_range(image_length, relocation_offset, size_of::<i32>())?;
            RelocationValue::Relative(relative)
        }
        unsupported => return Err(RelocationError::UnsupportedRelocation(unsupported)),
    };

    Ok(RelocationPatch {
        offset: relocation_offset,
        value,
    })
}

fn resolve_symbol(
    symbol: SymbolEntry,
    context: &RelocationContext<'_>,
) -> Result<u64, RelocationError> {
    match symbol.section_index {
        SECTION_UNDEFINED => {
            let name = symbol_name(context.strings, symbol.name_offset as usize)?;
            context
                .external_symbols
                .resolve(name)
                .ok_or(RelocationError::UnresolvedSymbol)
        }
        SECTION_ABSOLUTE => Ok(symbol.value),
        section_index => context
            .section_addresses
            .get(section_index as usize)
            .copied()
            .filter(|address| *address != 0)
            .and_then(|address| address.checked_add(symbol.value))
            .ok_or(RelocationError::InvalidSectionIndex),
    }
}

fn symbol_name(strings: &[u8], offset: usize) -> Result<&[u8], RelocationError> {
    let suffix = strings
        .get(offset..)
        .ok_or(RelocationError::InvalidString)?;
    let length = suffix
        .iter()
        .position(|byte| *byte == 0)
        .ok_or(RelocationError::InvalidString)?;
    Ok(&suffix[..length])
}

fn write_bytes(image: &mut [u8], offset: usize, value: &[u8]) -> Result<(), RelocationError> {
    let target = image
        .get_mut(offset..offset.saturating_add(value.len()))
        .ok_or(RelocationError::PatchOutsideImage)?;
    target.copy_from_slice(value);
    Ok(())
}

fn validate_patch_range(
    image_length: usize,
    offset: usize,
    width: usize,
) -> Result<(), RelocationError> {
    if offset
        .checked_add(width)
        .is_none_or(|end| end > image_length)
    {
        Err(RelocationError::PatchOutsideImage)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::*;

    struct TestResolver;

    impl ExternalSymbolResolver for TestResolver {
        fn resolve(&self, name: &[u8]) -> Option<u64> {
            (name == b"external").then_some(0x1020)
        }
    }

    struct OneShotResolver {
        calls: Cell<usize>,
    }

    impl ExternalSymbolResolver for OneShotResolver {
        fn resolve(&self, name: &[u8]) -> Option<u64> {
            let calls = self.calls.get();
            self.calls.set(calls + 1);
            (calls == 0 && name == b"external").then_some(0x1020)
        }
    }

    fn symbol(section_index: u16, value: u64) -> SymbolEntry {
        SymbolEntry {
            name_offset: 1,
            information: 0,
            visibility: 0,
            section_index,
            value,
            size: 0,
        }
    }

    #[test]
    fn applies_bounded_absolute_and_pc_relative_relocations() {
        let mut image = [0_u8; 16];
        let symbols = [symbol(SECTION_UNDEFINED, 0)];
        let relocations = [
            RelocationEntry {
                offset: 0,
                information: R_X86_64_64 as u64,
                addend: 4,
            },
            RelocationEntry {
                offset: 8,
                information: R_X86_64_PC32 as u64,
                addend: 0,
            },
        ];
        let context = RelocationContext {
            image_virtual_address: 0x1000,
            target_image_offset: 0,
            section_addresses: &[],
            symbols: &symbols,
            strings: b"\0external\0",
            external_symbols: &TestResolver,
        };
        apply_relocations(&mut image, &relocations, &context).unwrap();

        assert_eq!(u64::from_le_bytes(image[..8].try_into().unwrap()), 0x1024);
        assert_eq!(i32::from_le_bytes(image[8..12].try_into().unwrap()), 0x18);
    }

    #[test]
    fn rejects_unresolved_symbols_and_out_of_bounds_patches() {
        let symbols = [symbol(SECTION_UNDEFINED, 0)];
        let context = RelocationContext {
            image_virtual_address: 0,
            target_image_offset: 0,
            section_addresses: &[],
            symbols: &symbols,
            strings: b"\0missing\0",
            external_symbols: &TestResolver,
        };
        let relocation = RelocationEntry {
            offset: 0,
            information: R_X86_64_64 as u64,
            addend: 0,
        };
        assert_eq!(
            apply_relocations(&mut [0; 8], &[relocation], &context),
            Err(RelocationError::UnresolvedSymbol)
        );
    }

    #[test]
    fn rejects_the_complete_batch_before_patching_the_image() {
        let symbols = [symbol(SECTION_UNDEFINED, 0)];
        let context = RelocationContext {
            image_virtual_address: 0x1000,
            target_image_offset: 0,
            section_addresses: &[],
            symbols: &symbols,
            strings: b"\0external\0",
            external_symbols: &TestResolver,
        };
        let relocations = [
            RelocationEntry {
                offset: 0,
                information: R_X86_64_64 as u64,
                addend: 0,
            },
            RelocationEntry {
                offset: 8,
                information: 0xffff,
                addend: 0,
            },
        ];
        let mut image = [0x5a_u8; 16];
        let original = image;

        assert_eq!(
            apply_relocations(&mut image, &relocations, &context),
            Err(RelocationError::UnsupportedRelocation(0xffff))
        );
        assert_eq!(image, original);
    }

    #[test]
    fn resolves_each_external_binding_once_before_committing() {
        let symbols = [symbol(SECTION_UNDEFINED, 0)];
        let resolver = OneShotResolver {
            calls: Cell::new(0),
        };
        let context = RelocationContext {
            image_virtual_address: 0,
            target_image_offset: 0,
            section_addresses: &[],
            symbols: &symbols,
            strings: b"\0external\0",
            external_symbols: &resolver,
        };
        let relocation = RelocationEntry {
            offset: 0,
            information: R_X86_64_64 as u64,
            addend: 0,
        };
        let mut image = [0_u8; 8];

        apply_relocations(&mut image, &[relocation], &context).unwrap();

        assert_eq!(resolver.calls.get(), 1);
        assert_eq!(u64::from_le_bytes(image), 0x1020);
    }
}
