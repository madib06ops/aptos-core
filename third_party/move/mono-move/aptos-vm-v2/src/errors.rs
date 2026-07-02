// Copyright (c) Aptos Foundation
// Licensed pursuant to the Innovation-Enabling Source Code License, available at https://github.com/aptos-labs/aptos-core/blob/main/LICENSE

//! Maps MonoMove execution outcomes to `VMStatus`, including the special
//! treatment of prologue and epilogue failures.

use mono_move_core::LoaderError;
use mono_move_runtime::RuntimeError;
use move_core_types::vm_status::{AbortLocation, StatusCode, VMStatus};

// Move abort codes emitted by the transaction prologue, mirrored from
// `transaction_validation.move`.
const EBAD_ACCOUNT_AUTHENTICATION_KEY: u64 = 1001;
const ESEQUENCE_NUMBER_TOO_OLD: u64 = 1002;
const ESEQUENCE_NUMBER_TOO_NEW: u64 = 1003;
const EACCOUNT_DOES_NOT_EXIST: u64 = 1004;
const ECANT_PAY_GAS_DEPOSIT: u64 = 1005;
const ETRANSACTION_EXPIRED: u64 = 1006;
const EBAD_CHAIN_ID: u64 = 1007;
const ESEQUENCE_NUMBER_TOO_BIG: u64 = 1008;
const ESECONDARY_KEYS_ADDRESSES_COUNT_MISMATCH: u64 = 1009;
const EGAS_PAYER_ACCOUNT_MISSING: u64 = 1010;
const EINSUFFICIENT_BALANCE_FOR_REQUIRED_DEPOSIT: u64 = 1011;
const ENONCE_ALREADY_USED: u64 = 1012;
const ETRANSACTION_EXPIRATION_TOO_FAR_IN_FUTURE: u64 = 1013;

// Move error categories (see `error.move`).
const INVALID_ARGUMENT: u8 = 0x1;
const LIMIT_EXCEEDED: u8 = 0x2;
const INVALID_STATE: u8 = 0x3;

fn error_split(code: u64) -> (u8, u64) {
    let reason = code & 0xFFFF;
    let category = ((code >> 16) & 0xFF) as u8;
    (category, reason)
}

/// Converts a prologue Move abort into the validation status the transaction
/// is discarded with. Mirrors the legacy VM's `convert_prologue_error`.
//
// TODO(correctness): MonoMove aborts carry no location yet, so every prologue
// abort is treated as coming from the transaction-validation module. The
// multisig- and transaction-limits-module branches need locations to be
// distinguishable.
pub(crate) fn prologue_abort_to_status(code: u64, message: Option<String>) -> VMStatus {
    let new_major_status = match error_split(code) {
        (INVALID_ARGUMENT, EBAD_ACCOUNT_AUTHENTICATION_KEY) => StatusCode::INVALID_AUTH_KEY,
        (INVALID_ARGUMENT, ESEQUENCE_NUMBER_TOO_OLD) => StatusCode::SEQUENCE_NUMBER_TOO_OLD,
        (INVALID_ARGUMENT, ESEQUENCE_NUMBER_TOO_NEW) => StatusCode::SEQUENCE_NUMBER_TOO_NEW,
        (INVALID_ARGUMENT, EACCOUNT_DOES_NOT_EXIST) => StatusCode::SENDING_ACCOUNT_DOES_NOT_EXIST,
        (INVALID_ARGUMENT, ECANT_PAY_GAS_DEPOSIT) => {
            StatusCode::INSUFFICIENT_BALANCE_FOR_TRANSACTION_FEE
        },
        (INVALID_ARGUMENT, ETRANSACTION_EXPIRED) => StatusCode::TRANSACTION_EXPIRED,
        (INVALID_ARGUMENT, EBAD_CHAIN_ID) => StatusCode::BAD_CHAIN_ID,
        (LIMIT_EXCEEDED, ESEQUENCE_NUMBER_TOO_BIG) => StatusCode::SEQUENCE_NUMBER_TOO_BIG,
        (INVALID_ARGUMENT, ESECONDARY_KEYS_ADDRESSES_COUNT_MISMATCH) => {
            StatusCode::SECONDARY_KEYS_ADDRESSES_COUNT_MISMATCH
        },
        (INVALID_ARGUMENT, EGAS_PAYER_ACCOUNT_MISSING) => StatusCode::GAS_PAYER_ACCOUNT_MISSING,
        (INVALID_STATE, EINSUFFICIENT_BALANCE_FOR_REQUIRED_DEPOSIT) => {
            StatusCode::INSUFFICIENT_BALANCE_FOR_REQUIRED_DEPOSIT
        },
        (INVALID_ARGUMENT, ETRANSACTION_EXPIRATION_TOO_FAR_IN_FUTURE) => {
            StatusCode::TRANSACTION_EXPIRATION_TOO_FAR_IN_FUTURE
        },
        (INVALID_ARGUMENT, ENONCE_ALREADY_USED) => StatusCode::NONCE_ALREADY_USED,
        (category, reason) => {
            let mut err_msg = format!(
                "[aptos_vm_v2] Unexpected prologue Move abort: {:?} (Category: {:?} Reason: {:?})",
                code, category, reason
            );
            if let Some(abort_msg) = message {
                err_msg.push_str(" Message: ");
                err_msg.push_str(&abort_msg);
            }
            return VMStatus::error(
                StatusCode::UNEXPECTED_ERROR_FROM_KNOWN_MOVE_FUNCTION,
                Some(err_msg),
            );
        },
    };
    VMStatus::error(new_major_status, None)
}

/// A non-abort failure while running the prologue or epilogue: these functions
/// must not fail, so anything else is an invariant violation.
pub(crate) fn unexpected_validation_error(what: &str, detail: String) -> VMStatus {
    VMStatus::error(
        StatusCode::UNEXPECTED_ERROR_FROM_KNOWN_MOVE_FUNCTION,
        Some(format!("[aptos_vm_v2] Unexpected {what} error: {detail}")),
    )
}

/// A user-payload Move abort.
//
// TODO(correctness): MonoMove does not carry the aborting module yet, so the
// location is `Script`; the legacy VM reports `Module(..)` for module aborts.
pub(crate) fn payload_abort_status(code: u64, message: Option<String>) -> VMStatus {
    VMStatus::MoveAbort {
        location: AbortLocation::Script,
        code,
        message,
    }
}

/// Maps a payload [`RuntimeError`] to a `VMStatus`.
//
// TODO(correctness): the mapping is coarse; several kinds need location info
// (`ExecutionFailure`) and exact status codes to match the legacy VM.
pub(crate) fn runtime_error_to_status(err: RuntimeError) -> VMStatus {
    use RuntimeError as E;
    let code = match &err {
        E::GasExhausted(_) => StatusCode::OUT_OF_GAS,
        E::ArithmeticOverflow { .. }
        | E::ArithmeticUnderflow { .. }
        | E::DivisionByZero { .. }
        | E::ShiftAmountOutOfRange { .. }
        | E::ArithmeticUnderOverflow { .. }
        | E::DivisionByZeroOrOverflow { .. }
        | E::NegateMinOverflow { .. }
        | E::CastOutOfRange { .. } => StatusCode::ARITHMETIC_ERROR,
        E::PopFromEmptyVector
        | E::VectorIndexOutOfBounds { .. }
        | E::VecUnpackLengthMismatch { .. } => StatusCode::VECTOR_OPERATION_ERROR,
        E::ResourceDoesNotExist { .. } => StatusCode::MISSING_DATA,
        E::ResourceAlreadyExists { .. } => StatusCode::RESOURCE_ALREADY_EXISTS,
        E::EnumVariantMismatch { .. } => StatusCode::TYPE_MISMATCH,
        E::StackOverflow => StatusCode::CALL_STACK_OVERFLOW,
        E::OutOfHeapMemory { .. } | E::AllocationTooLarge { .. } | E::VecAllocSizeOverflow => {
            StatusCode::MEMORY_LIMIT_EXCEEDED
        },
        E::InvalidAbortMessage
        | E::AbortMessageTooLong { .. }
        | E::BCSEof
        | E::BCSInvalidUleb
        | E::BCSSequenceTooLong { .. }
        | E::BCSRemainingInput { .. }
        | E::BCSInvalidBool { .. } => StatusCode::UNKNOWN_STATUS,
        E::Loader(loader_err) => return loader_error_to_status(loader_err),
        E::InvariantViolation(_) | E::ResourceProvider(_) => {
            StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR
        },
    };
    VMStatus::error(code, Some(format!("{}", err)))
}

fn loader_error_to_status(err: &LoaderError) -> VMStatus {
    use LoaderError as L;
    let code = match err {
        L::GasExhausted(_) => StatusCode::OUT_OF_GAS,
        L::ModuleNotFound { .. } | L::FunctionNotFound { .. } | L::FunctionIrMissing => {
            StatusCode::LINKER_ERROR
        },
        L::Deserialization(_) => StatusCode::CODE_DESERIALIZATION_ERROR,
        L::Verification(_) => StatusCode::VERIFICATION_ERROR,
        L::LoweringSkipped { .. } | L::ModuleProvider(_) | L::Specializer(_) => {
            StatusCode::UNKNOWN_STATUS
        },
        L::GlobalContext(_) | L::InvariantViolation(_) => {
            StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR
        },
    };
    VMStatus::error(code, Some(format!("{}", err)))
}
