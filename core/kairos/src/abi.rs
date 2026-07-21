pub const ABI_MAGIC: u32 = 0x4b41_494f;
pub const ABI_VERSION: u16 = 1;
pub const ENDIAN_LITTLE: u8 = 1;
pub const ENDIAN_BIG: u8 = 2;
pub const ABI_KIND_NATIVE: u8 = 1;
pub const ABI_KIND_DRIVER: u8 = 2;
pub const ABI_KIND_HYPERCALL: u8 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct AbiDescriptor {
    pub magic: u32,
    pub version: u16,
    pub structure_size: u16,
    pub endian: u8,
    pub word_bits: u8,
    pub pointer_bits: u8,
    pub abi_kind: u8,
    pub page_size: u32,
    pub syscall_style: u16,
    pub object_handle_bits: u16,
    pub features_low: u64,
    pub features_high: u64,
}

impl AbiDescriptor {
    pub const fn native(features_low: u64, features_high: u64) -> Self {
        Self {
            magic: ABI_MAGIC,
            version: ABI_VERSION,
            structure_size: core::mem::size_of::<Self>() as u16,
            endian: if cfg!(target_endian = "little") {
                ENDIAN_LITTLE
            } else {
                ENDIAN_BIG
            },
            word_bits: usize::BITS as u8,
            pointer_bits: usize::BITS as u8,
            abi_kind: ABI_KIND_NATIVE,
            page_size: 4096,
            syscall_style: 1,
            object_handle_bits: 64,
            features_low,
            features_high,
        }
    }

    pub fn validate(self) -> Result<(), NegotiationError> {
        if self.magic != ABI_MAGIC
            || self.version != ABI_VERSION
            || usize::from(self.structure_size) != core::mem::size_of::<Self>()
        {
            return Err(NegotiationError::InvalidDescriptor);
        }
        if !matches!(self.endian, ENDIAN_LITTLE | ENDIAN_BIG)
            || !matches!(self.word_bits, 32 | 64)
            || !matches!(self.pointer_bits, 32 | 64)
            || self.pointer_bits > self.word_bits
            || !matches!(
                self.abi_kind,
                ABI_KIND_NATIVE | ABI_KIND_DRIVER | ABI_KIND_HYPERCALL
            )
            || !self.page_size.is_power_of_two()
            || !(4096..=65_536).contains(&self.page_size)
        {
            return Err(NegotiationError::InvalidDescriptor);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NegotiatedAbi {
    pub descriptor: AbiDescriptor,
    pub unavailable_features_low: u64,
    pub unavailable_features_high: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NegotiationError {
    InvalidDescriptor,
    IncompatibleEndian,
    IncompatibleWordSize,
    IncompatiblePageSize,
    UnsupportedKind,
}

pub fn negotiate(
    native: AbiDescriptor,
    requested: AbiDescriptor,
) -> Result<NegotiatedAbi, NegotiationError> {
    native.validate()?;
    requested.validate()?;
    if requested.endian != native.endian {
        return Err(NegotiationError::IncompatibleEndian);
    }
    if requested.word_bits != native.word_bits || requested.pointer_bits != native.pointer_bits {
        return Err(NegotiationError::IncompatibleWordSize);
    }
    if requested.page_size != native.page_size {
        return Err(NegotiationError::IncompatiblePageSize);
    }
    if requested.abi_kind != ABI_KIND_NATIVE && requested.abi_kind != ABI_KIND_DRIVER {
        return Err(NegotiationError::UnsupportedKind);
    }
    let unavailable_features_low = requested.features_low & !native.features_low;
    let unavailable_features_high = requested.features_high & !native.features_high;
    let mut descriptor = native;
    descriptor.abi_kind = requested.abi_kind;
    descriptor.features_low &= requested.features_low;
    descriptor.features_high &= requested.features_high;
    Ok(NegotiatedAbi {
        descriptor,
        unavailable_features_low,
        unavailable_features_high,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiates_only_the_feature_intersection() {
        let native = AbiDescriptor::native(0b1011, 0);
        let mut requested = native;
        requested.abi_kind = ABI_KIND_DRIVER;
        requested.features_low = 0b1110;
        let result = negotiate(native, requested).unwrap();
        assert_eq!(result.descriptor.features_low, 0b1010);
        assert_eq!(result.unavailable_features_low, 0b0100);
    }

    #[test]
    fn rejects_layout_incompatibilities() {
        let native = AbiDescriptor::native(0, 0);
        let mut requested = native;
        requested.page_size = 16_384;
        assert_eq!(
            negotiate(native, requested),
            Err(NegotiationError::IncompatiblePageSize)
        );
        requested = native;
        requested.structure_size = 0;
        assert_eq!(
            negotiate(native, requested),
            Err(NegotiationError::InvalidDescriptor)
        );
    }
}
