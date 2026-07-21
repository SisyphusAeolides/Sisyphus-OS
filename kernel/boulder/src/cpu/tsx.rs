use crate::memory::transaction::{TransactionAbort, rtm_available};

pub use crate::memory::transaction::TransactionError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TsxQueryError {
    Unsupported,
}

pub fn is_in_transaction() -> Result<bool, TsxQueryError> {
    if !rtm_available() {
        return Err(TsxQueryError::Unsupported);
    }
    let mut active: u8;
    unsafe {
        core::arch::asm!(
            "xtest",
            "setnz {active}",
            active = out(reg_byte) active,
            options(nomem, nostack),
        );
    }
    Ok(active != 0)
}

pub const fn decode_abort_status(status: u32) -> TransactionAbort {
    TransactionAbort::from_raw(status)
}
