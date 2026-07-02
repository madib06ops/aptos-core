// Copyright (c) Aptos Foundation
// Licensed pursuant to the Innovation-Enabling Source Code License, available at https://github.com/aptos-labs/aptos-core/blob/main/LICENSE

//! Sequential block execution: runs each user transaction in order against an
//! overlay of the outputs before it.
//
// TODO(completeness): Block-STM integration (parallel execution) is a separate
// workstream; this wrapper is the single-threaded stand-in.

use crate::AptosVMv2;
use aptos_block_executor::txn_provider::TxnProvider;
use aptos_types::{
    block_executor::{
        config::BlockExecutorConfigFromOnchain,
        transaction_slice_metadata::TransactionSliceMetadata,
    },
    state_store::{
        state_key::StateKey, state_storage_usage::StateStorageUsage, state_value::StateValue,
        StateView, StateViewResult, TStateView,
    },
    transaction::{
        signature_verified_transaction::SignatureVerifiedTransaction, AuxiliaryInfo, BlockOutput,
        Transaction, TransactionOutput, TransactionStatus,
    },
    write_set::TransactionWrite,
};
use aptos_vm::VMBlockExecutor;
use aptos_vm_types::output::VMOutput;
use move_core_types::vm_status::{StatusCode, VMStatus};
use std::collections::HashMap;

/// `VMBlockExecutor` implementation backed by MonoMove, executing sequentially.
pub struct AptosVMv2BlockExecutor {
    vm: AptosVMv2,
}

impl VMBlockExecutor for AptosVMv2BlockExecutor {
    fn new() -> Self {
        Self {
            vm: AptosVMv2::new(),
        }
    }

    fn execute_block(
        &self,
        txn_provider: &aptos_block_executor::txn_provider::default::DefaultTxnProvider<
            SignatureVerifiedTransaction,
            AuxiliaryInfo,
        >,
        state_view: &(impl StateView + Sync),
        _onchain_config: BlockExecutorConfigFromOnchain,
        _transaction_slice_metadata: TransactionSliceMetadata,
    ) -> Result<BlockOutput<SignatureVerifiedTransaction, TransactionOutput>, VMStatus> {
        let mut overlay = OverlayView {
            base: state_view,
            overlay: HashMap::new(),
        };
        let mut outputs = Vec::with_capacity(txn_provider.num_txns());
        for txn in txn_provider.get_txns() {
            let output = match txn.expect_valid() {
                Transaction::UserTransaction(signed_txn) => {
                    let (_status, output) = self.vm.execute_user_transaction(&overlay, signed_txn);
                    output
                },
                // TODO(completeness): block metadata, epilogue, state
                // checkpoint, and validator transactions.
                _ => VMOutput::empty_with_status(TransactionStatus::Discard(
                    StatusCode::FEATURE_UNDER_GATING,
                )),
            };
            let output = output
                .try_materialize_into_transaction_output(&overlay)
                .map_err(|e| {
                    VMStatus::error(
                        StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR,
                        Some(format!("materialization failed: {e:?}")),
                    )
                })?;
            for (key, op) in output.write_set().write_op_iter() {
                overlay.overlay.insert(key.clone(), op.as_state_value());
            }
            outputs.push(output);
        }
        Ok(BlockOutput::new(outputs, None))
    }
}

/// A state view layering the block's prior outputs over the base view.
struct OverlayView<'a, S> {
    base: &'a S,
    overlay: HashMap<StateKey, Option<StateValue>>,
}

impl<S: StateView> TStateView for OverlayView<'_, S> {
    type Key = StateKey;

    fn get_state_value(&self, state_key: &StateKey) -> StateViewResult<Option<StateValue>> {
        match self.overlay.get(state_key) {
            Some(value) => Ok(value.clone()),
            None => self.base.get_state_value(state_key),
        }
    }

    fn get_usage(&self) -> StateViewResult<StateStorageUsage> {
        // TODO(correctness): account for the overlay's items and bytes.
        self.base.get_usage()
    }
}
