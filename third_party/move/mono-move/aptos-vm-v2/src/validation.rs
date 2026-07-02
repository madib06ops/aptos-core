// Copyright (c) Aptos Foundation
// Licensed pursuant to the Innovation-Enabling Source Code License, available at https://github.com/aptos-labs/aptos-core/blob/main/LICENSE

//! Runs the transaction prologue and epilogue (`0x1::transaction_validation`).
//! Only the versioned validation path is supported; the many legacy
//! prologue/epilogue variants are intentionally not ported.
//
// TODO(completeness): reject transactions whose on-chain feature set disables
// versioned validation instead of assuming it.

use crate::{
    errors::{prologue_abort_to_status, unexpected_validation_error},
    metadata::TxnMetadata,
    session::{call_function, CallOutcome, Interp},
};
use aptos_types::{
    fee_statement::FeeStatement,
    transaction::{ReplayProtector, UserTxnLimitsRequest},
};
use mono_move_core::Interner;
use mono_move_global_context::ExecutionGuard;
use move_binary_format::CompiledModule;
use move_core_types::{
    account_address::AccountAddress, ident_str, identifier::IdentStr, vm_status::VMStatus,
};
use serde::Serialize;

const TRANSACTION_VALIDATION: &IdentStr = ident_str!("transaction_validation");
const VERSIONED_PROLOGUE: &IdentStr = ident_str!("versioned_prologue");
const VERSIONED_EPILOGUE: &IdentStr = ident_str!("versioned_epilogue");

/// Prologue and epilogue run unmetered, mirroring the legacy VM.
const UNMETERED: u64 = u64::MAX;

/// Mirrors the Move enum in `transaction_validation.move`; must keep the same
/// BCS serialization (see the legacy VM's `PrologueArgs`).
#[derive(Serialize)]
enum PrologueArgs {
    V1 {
        needs_fee_payer_auth_check: bool,
        txn_sender_public_key: Option<Vec<u8>>,
        fee_payer_public_key_hash: Option<Vec<u8>>,
        replay_protector: ReplayProtector,
        secondary_signer_addresses: Vec<AccountAddress>,
        secondary_signer_public_key_hashes: Vec<Option<Vec<u8>>>,
        txn_gas_price: u64,
        txn_max_gas_units: u64,
        txn_expiration_time: u64,
        chain_id: u8,
        is_simulation: bool,
        txn_limits_request: Option<UserTxnLimitsRequest>,
    },
}

/// Mirrors the Move enum in `transaction_validation.move`; must keep the same
/// BCS serialization (see the legacy VM's `EpilogueArgs`).
#[derive(Serialize)]
enum EpilogueArgs {
    V1 {
        fee_statement: FeeStatement,
        txn_gas_price: u64,
        txn_max_gas_units: u64,
        gas_units_remaining: u64,
        is_simulation: bool,
        is_orderless_txn: bool,
    },
}

/// The sender and fee-payer signer buffers every validation call takes.
/// Returned as owned arrays so `&signer` fat pointers can reference them for
/// the whole transaction.
pub(crate) fn validation_signers(txn_data: &TxnMetadata) -> [[u8; AccountAddress::LENGTH]; 2] {
    let fee_payer = txn_data.fee_payer.unwrap_or(txn_data.sender);
    [txn_data.sender.into_bytes(), fee_payer.into_bytes()]
}

pub(crate) fn run_prologue(
    interp: &mut Interp<'_, '_, '_>,
    guard: &ExecutionGuard<'_>,
    validation_module: &CompiledModule,
    signers: &[[u8; AccountAddress::LENGTH]; 2],
    txn_data: &TxnMetadata,
) -> Result<(), VMStatus> {
    let args = PrologueArgs::V1 {
        needs_fee_payer_auth_check: txn_data.fee_payer.is_some(),
        txn_sender_public_key: txn_data.sender_auth_key.clone(),
        fee_payer_public_key_hash: txn_data.fee_payer_auth_key.clone(),
        replay_protector: txn_data.replay_protector,
        secondary_signer_addresses: txn_data.secondary_signers.clone(),
        secondary_signer_public_key_hashes: txn_data.secondary_auth_keys.clone(),
        txn_gas_price: txn_data.gas_unit_price,
        txn_max_gas_units: txn_data.max_gas_amount,
        txn_expiration_time: txn_data.expiration_timestamp_secs,
        chain_id: txn_data.chain_id,
        is_simulation: false,
        // TODO(completeness): transaction limits requests (staking multipliers).
        txn_limits_request: None,
    };
    let args_blob = bcs::to_bytes(&args).expect("prologue args serialize");

    let outcome = call_function(
        interp,
        guard,
        validation_module,
        &AccountAddress::ONE,
        TRANSACTION_VALIDATION,
        VERSIONED_PROLOGUE,
        guard.type_list_of(&[]),
        signers,
        &[args_blob],
        UNMETERED,
    )
    .map_err(|err| unexpected_validation_error("prologue", err.to_string()))?;

    match outcome {
        CallOutcome::Success => Ok(()),
        CallOutcome::Abort { code, message } => Err(prologue_abort_to_status(code, message)),
        CallOutcome::Error(err) => Err(unexpected_validation_error("prologue", err.to_string())),
    }
}

pub(crate) fn run_epilogue(
    interp: &mut Interp<'_, '_, '_>,
    guard: &ExecutionGuard<'_>,
    validation_module: &CompiledModule,
    signers: &[[u8; AccountAddress::LENGTH]; 2],
    txn_data: &TxnMetadata,
    fee_statement: FeeStatement,
    gas_units_remaining: u64,
) -> Result<(), VMStatus> {
    let args = EpilogueArgs::V1 {
        fee_statement,
        txn_gas_price: txn_data.gas_unit_price,
        txn_max_gas_units: txn_data.max_gas_amount,
        gas_units_remaining,
        is_simulation: false,
        is_orderless_txn: txn_data.is_orderless(),
    };
    let args_blob = bcs::to_bytes(&args).expect("epilogue args serialize");

    let outcome = call_function(
        interp,
        guard,
        validation_module,
        &AccountAddress::ONE,
        TRANSACTION_VALIDATION,
        VERSIONED_EPILOGUE,
        guard.type_list_of(&[]),
        signers,
        &[args_blob],
        UNMETERED,
    )
    .map_err(|err| unexpected_validation_error("epilogue", err.to_string()))?;

    match outcome {
        CallOutcome::Success => Ok(()),
        // The epilogue must not fail; the legacy VM maps any failure to an
        // invariant violation.
        // TODO(correctness): the legacy VM keeps one insufficient-balance
        // abort code as a Move abort instead.
        CallOutcome::Abort { code, message } => Err(unexpected_validation_error(
            "epilogue",
            format!("abort {code}: {message:?}"),
        )),
        CallOutcome::Error(err) => Err(unexpected_validation_error("epilogue", err.to_string())),
    }
}
