#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum X64Abi {
    SystemV,
    Microsoft,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScalarCallContract {
    pub integer_arguments: u8,
    pub has_vector_arguments: bool,
    pub variadic: bool,
    pub may_unwind: bool,
    pub target_has_endbr64: bool,
    pub emit_endbr64: bool,
}

impl ScalarCallContract {
    pub const fn hermes_callback(integer_arguments: u8) -> Self {
        Self {
            integer_arguments,
            has_vector_arguments: false,
            variadic: false,
            may_unwind: false,
            target_has_endbr64: false,
            emit_endbr64: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MorphicError {
    InvalidTarget,
    OutputTooSmall,
    UnsupportedContract,
    UnsupportedArchitecturePolicy,
}

struct CodeSink<'a> {
    bytes: &'a mut [u8],
    used: usize,
}

impl<'a> CodeSink<'a> {
    fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes, used: 0 }
    }

    fn emit(&mut self, bytes: &[u8]) -> Result<(), MorphicError> {
        let end = self
            .used
            .checked_add(bytes.len())
            .ok_or(MorphicError::OutputTooSmall)?;
        let destination = self
            .bytes
            .get_mut(self.used..end)
            .ok_or(MorphicError::OutputTooSmall)?;
        destination.copy_from_slice(bytes);
        self.used = end;
        Ok(())
    }

    fn emit_u64(&mut self, value: u64) -> Result<(), MorphicError> {
        self.emit(&value.to_le_bytes())
    }

    const fn used(&self) -> usize {
        self.used
    }
}

/// Emits a bounded x86-64 scalar ABI bridge.
///
/// `source` is the calling convention used by the caller entering the thunk.
/// `target` is the calling convention expected by `target_address`.
///
/// The contract is deliberately narrow:
/// - zero through four integer or pointer arguments;
/// - scalar integer or pointer return in RAX;
/// - no variadic arguments;
/// - no vector arguments;
/// - no unwinding through the thunk;
/// - no stack arguments.
///
/// A foreign module must provide an explicit function contract. Prologue
/// classification may corroborate that contract, but cannot replace it.
pub fn emit_scalar_bridge(
    output: &mut [u8],
    source: X64Abi,
    target: X64Abi,
    target_address: u64,
    contract: ScalarCallContract,
) -> Result<usize, MorphicError> {
    if target_address == 0 {
        return Err(MorphicError::InvalidTarget);
    }

    if contract.integer_arguments > 4
        || contract.has_vector_arguments
        || contract.variadic
        || contract.may_unwind
    {
        return Err(MorphicError::UnsupportedContract);
    }

    if contract.emit_endbr64 && !contract.target_has_endbr64 && source != target {
        return Err(MorphicError::UnsupportedArchitecturePolicy);
    }

    let mut sink = CodeSink::new(output);

    if contract.emit_endbr64 {
        sink.emit(&[0xf3, 0x0f, 0x1e, 0xfa])?;
    }

    match (source, target) {
        (X64Abi::SystemV, X64Abi::SystemV) | (X64Abi::Microsoft, X64Abi::Microsoft) => {
            emit_absolute_tail_jump(&mut sink, target_address)?;
        }
        (X64Abi::SystemV, X64Abi::Microsoft) => {
            emit_systemv_to_microsoft(&mut sink, target_address, contract.integer_arguments)?;
        }
        (X64Abi::Microsoft, X64Abi::SystemV) => {
            emit_microsoft_to_systemv(&mut sink, target_address, contract.integer_arguments)?;
        }
    }

    Ok(sink.used())
}

fn emit_absolute_tail_jump(
    sink: &mut CodeSink<'_>,
    target_address: u64,
) -> Result<(), MorphicError> {
    // mov rax, imm64
    sink.emit(&[0x48, 0xb8])?;
    sink.emit_u64(target_address)?;
    // jmp rax
    sink.emit(&[0xff, 0xe0])
}

fn emit_systemv_to_microsoft(
    sink: &mut CodeSink<'_>,
    target_address: u64,
    arguments: u8,
) -> Result<(), MorphicError> {
    // SysV:       RDI, RSI, RDX, RCX
    // Microsoft:  RCX, RDX, R8,  R9
    //
    // R10 and R11 are volatile in both conventions and form the permutation
    // buffer for arguments three and four.
    if arguments >= 3 {
        // mov r10, rdx
        sink.emit(&[0x49, 0x89, 0xd2])?;
    }
    if arguments >= 4 {
        // mov r11, rcx
        sink.emit(&[0x49, 0x89, 0xcb])?;
    }
    if arguments >= 1 {
        // mov rcx, rdi
        sink.emit(&[0x48, 0x89, 0xf9])?;
    }
    if arguments >= 2 {
        // mov rdx, rsi
        sink.emit(&[0x48, 0x89, 0xf2])?;
    }
    if arguments >= 3 {
        // mov r8, r10
        sink.emit(&[0x4d, 0x89, 0xd0])?;
    }
    if arguments >= 4 {
        // mov r9, r11
        sink.emit(&[0x4d, 0x89, 0xd9])?;
    }

    // Reserve 32 bytes of Microsoft shadow space plus eight bytes for
    // call-site alignment.
    sink.emit(&[0x48, 0x83, 0xec, 0x28])?;

    // mov rax, imm64
    sink.emit(&[0x48, 0xb8])?;
    sink.emit_u64(target_address)?;

    // call rax
    sink.emit(&[0xff, 0xd0])?;

    // add rsp, 40
    sink.emit(&[0x48, 0x83, 0xc4, 0x28])?;
    // ret
    sink.emit(&[0xc3])
}

fn emit_microsoft_to_systemv(
    sink: &mut CodeSink<'_>,
    target_address: u64,
    arguments: u8,
) -> Result<(), MorphicError> {
    // Microsoft treats RDI and RSI as nonvolatile. Preserve both before using
    // them as System V argument registers.
    // push rdi
    sink.emit(&[0x57])?;
    // push rsi
    sink.emit(&[0x56])?;

    if arguments >= 1 {
        // mov rdi, rcx
        sink.emit(&[0x48, 0x89, 0xcf])?;
    }
    if arguments >= 2 {
        // mov rsi, rdx
        sink.emit(&[0x48, 0x89, 0xd6])?;
    }
    if arguments >= 3 {
        // mov rdx, r8
        sink.emit(&[0x4c, 0x89, 0xc2])?;
    }
    if arguments >= 4 {
        // mov rcx, r9
        sink.emit(&[0x4c, 0x89, 0xc9])?;
    }

    // After two pushes, reserve eight bytes to restore 16-byte alignment at
    // the System V call site.
    sink.emit(&[0x48, 0x83, 0xec, 0x08])?;

    // mov rax, imm64
    sink.emit(&[0x48, 0xb8])?;
    sink.emit_u64(target_address)?;

    // call rax
    sink.emit(&[0xff, 0xd0])?;

    // add rsp, 8
    sink.emit(&[0x48, 0x83, 0xc4, 0x08])?;
    // pop rsi
    sink.emit(&[0x5e])?;
    // pop rdi
    sink.emit(&[0x5f])?;
    // ret
    sink.emit(&[0xc3])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_abi_is_an_absolute_tail_jump() {
        let mut output = [0_u8; 64];
        let used = emit_scalar_bridge(
            &mut output,
            X64Abi::SystemV,
            X64Abi::SystemV,
            0x1122_3344_5566_7788,
            ScalarCallContract::hermes_callback(2),
        )
        .unwrap();

        assert_eq!(used, 12);
        assert_eq!(&output[..2], &[0x48, 0xb8]);
        assert_eq!(&output[10..12], &[0xff, 0xe0]);
    }

    #[test]
    fn systemv_to_microsoft_reserves_shadow_space() {
        let mut output = [0_u8; 96];
        let used = emit_scalar_bridge(
            &mut output,
            X64Abi::SystemV,
            X64Abi::Microsoft,
            0x1234,
            ScalarCallContract::hermes_callback(4),
        )
        .unwrap();

        assert!(used > 20);
        assert!(
            output[..used]
                .windows(4)
                .any(|window| window == [0x48, 0x83, 0xec, 0x28])
        );
    }

    #[test]
    fn microsoft_to_systemv_preserves_rdi_and_rsi() {
        let mut output = [0_u8; 96];
        let used = emit_scalar_bridge(
            &mut output,
            X64Abi::Microsoft,
            X64Abi::SystemV,
            0x1234,
            ScalarCallContract::hermes_callback(4),
        )
        .unwrap();

        assert_eq!(&output[..2], &[0x57, 0x56]);
        assert_eq!(&output[used - 3..used], &[0x5e, 0x5f, 0xc3]);
    }

    #[test]
    fn rejects_stack_or_vector_contracts() {
        let mut output = [0_u8; 96];
        let mut contract = ScalarCallContract::hermes_callback(5);
        assert_eq!(
            emit_scalar_bridge(&mut output, X64Abi::SystemV, X64Abi::Microsoft, 1, contract,),
            Err(MorphicError::UnsupportedContract)
        );

        contract.integer_arguments = 4;
        contract.has_vector_arguments = true;
        assert_eq!(
            emit_scalar_bridge(&mut output, X64Abi::SystemV, X64Abi::Microsoft, 1, contract,),
            Err(MorphicError::UnsupportedContract)
        );
    }
}
