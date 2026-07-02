// Copyright (c) Aptos Foundation
// Licensed pursuant to the Innovation-Enabling Source Code License, available at https://github.com/aptos-labs/aptos-core/blob/main/LICENSE

//! The AptosVM transaction-execution layer reimplemented on the MonoMove VM.
//!
//! Executes user transactions single-threaded — prologue, payload, epilogue,
//! write-set publication — producing a `VMOutput`. One MonoMove session with
//! checkpoints replaces the legacy VM's staged sessions: a failed payload
//! rolls the heap and read-write set back to the post-prologue state, and
//! writes are drained exactly once at the end.

mod block_executor;
mod convert;
mod errors;
mod gas;
mod metadata;
mod providers;
mod publish;
mod session;
mod types;
mod validation;

use crate::{
    convert::{finalize_events, into_vm_output},
    errors::{payload_abort_status, runtime_error_to_status},
    gas::TxnGas,
    metadata::TxnMetadata,
    providers::{StateViewModuleProvider, StateViewResourceProvider},
    publish::drain_write_set,
    session::{build_natives, call_function, CallOutcome, Interp},
    validation::{run_epilogue, run_prologue, validation_signers},
};
use aptos_types::{
    on_chain_config::{FeatureFlag, Features, OnChainConfig},
    state_store::StateView,
    transaction::{EntryFunction, SignedTransaction, TransactionExecutableRef, TransactionStatus},
};
use aptos_vm_types::output::VMOutput;
pub use block_executor::AptosVMv2BlockExecutor;
use bytes::Bytes;
use mono_move_core::{
    native::NativeExtensions, storage::module_provider::ModuleProvider, GasMeter, Interner,
};
use mono_move_global_context::{ExecutionGuard, GlobalContext};
use mono_move_loader::{Loader, LoadingPolicy, LoweringPolicy};
use mono_move_natives::{
    EventStore, ObjectContextExtension, StorageUsageAtEpochBoundary, TransactionContextExtension,
};
use mono_move_runtime::{ExecutionContext, InterpreterContext, TransactionContext};
use move_binary_format::CompiledModule;
use move_core_types::{
    account_address::AccountAddress,
    ident_str,
    vm_status::{StatusCode, VMStatus},
};

/// Size of the arena resources are materialized into.
// TODO(metering): size this from the transaction's memory limits instead.
const RESOURCE_ARENA_BYTES: usize = 64 * 1024 * 1024;

/// The AptosVM reimplemented on MonoMove. Owns the cross-transaction code
/// cache; execution itself is single-threaded.
pub struct AptosVMv2 {
    global_ctx: GlobalContext,
    // TODO(completeness): load and honor the `AptosEnvironment` (features, gas
    // parameters, VM configs) instead of assuming the current mainnet feature
    // set.
}

impl AptosVMv2 {
    pub fn new() -> Self {
        Self {
            global_ctx: GlobalContext::with_num_execution_workers(1),
        }
    }

    /// Executes one user transaction against the state view, returning the
    /// detailed status and the transaction's output.
    ///
    /// Signature verification is the caller's responsibility, mirroring the
    /// legacy VM (which receives signature-verified transactions).
    //
    // TODO(completeness): take Block-STM's per-transaction view traits
    // (`ExecutorView` + `ResourceGroupView`) instead of a raw `StateView`.
    pub fn execute_user_transaction<S: StateView>(
        &self,
        state_view: &S,
        txn: &SignedTransaction,
    ) -> (VMStatus, VMOutput) {
        match self.execute_user_transaction_impl(state_view, txn) {
            Ok((status, output)) => (status, output),
            Err(status) => {
                let output =
                    VMOutput::empty_with_status(TransactionStatus::Discard(status.status_code()));
                (status, output)
            },
        }
    }

    /// The transaction spine. `Err` means the transaction is discarded with no
    /// output; `Ok` carries a kept (or informative discarded) output.
    fn execute_user_transaction_impl<S: StateView>(
        &self,
        state_view: &S,
        txn: &SignedTransaction,
    ) -> Result<(VMStatus, VMOutput), VMStatus> {
        let features = Features::fetch_config(state_view)
            .ok()
            .flatten()
            .unwrap_or_default();
        if !features.is_enabled(FeatureFlag::VERSIONED_TRANSACTION_VALIDATION) {
            // TODO(completeness): the many legacy validation paths.
            return Err(VMStatus::error(
                StatusCode::FEATURE_UNDER_GATING,
                Some("aptos-vm-v2 only supports versioned transaction validation".to_string()),
            ));
        }

        let txn_data = TxnMetadata::new(txn);
        // TODO(metering): pre-flight gas checks (`check_gas`): txn size bounds,
        // gas price bounds, intrinsic gas coverage.

        let guard = self.global_ctx.try_execution_context(0).ok_or_else(|| {
            VMStatus::error(
                StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR,
                Some("failed to acquire MonoMove execution context".to_string()),
            )
        })?;

        let module_provider = StateViewModuleProvider::new(state_view);
        let natives = build_natives(&guard);
        let loader = Loader::new_with_policy(
            &guard,
            &module_provider,
            LoadingPolicy::Lazy(LoweringPolicy::Lazy),
            &natives,
        );
        let resource_provider =
            StateViewResourceProvider::new(&guard, state_view, RESOURCE_ARENA_BYTES);

        let mut extensions = NativeExtensions::new();
        extensions.add(TransactionContextExtension::new(
            txn_data.txn_hash.clone(),
            0,
            // TODO(correctness): thread the real transaction index once block
            // execution provides one.
            0,
            0,
        ));
        extensions.add(ObjectContextExtension::new());
        let usage = state_view.get_usage().map_err(|e| {
            VMStatus::error(
                StatusCode::STORAGE_ERROR,
                Some(format!("failed to read state usage: {e}")),
            )
        })?;
        extensions.add(StorageUsageAtEpochBoundary::new(
            usage.items() as u64,
            usage.bytes() as u64,
        ));
        extensions.add(EventStore::new());

        let mut txn_ctx = TransactionContext::new(
            loader,
            GasMeter::with_max_budget(),
            &resource_provider,
            &natives,
        )
        .with_extensions(extensions);

        // The validation module's compiled form, for classifying prologue and
        // epilogue parameters.
        let validation_module = fetch_module(
            &module_provider,
            &AccountAddress::ONE,
            "transaction_validation",
        )
        .map_err(|e| VMStatus::error(StatusCode::LINKER_ERROR, Some(format!("{e:#}"))))?;

        // Bootstrap the interpreter on the prologue; each subsequent call
        // re-targets it while keeping the heap, read-write set, and extensions.
        let prologue_ptr = {
            let module_id = guard
                .intern_address_name(&AccountAddress::ONE, ident_str!("transaction_validation"))
                .into_global_arena_ptr();
            let function = guard
                .intern_identifier(ident_str!("versioned_prologue"))
                .into_global_arena_ptr();
            txn_ctx
                .load_function(module_id, function, guard.type_list_of(&[]))
                .map_err(|e| VMStatus::error(StatusCode::LINKER_ERROR, Some(format!("{e}"))))?
        };
        // SAFETY: the pointer lives in a LoadedModule arena kept alive by the guard.
        let prologue_func = unsafe { prologue_ptr.as_ref_unchecked() };
        let mut interp = InterpreterContext::new(&mut txn_ctx, prologue_func);

        let signers = validation_signers(&txn_data);

        // Prologue. Failure discards the transaction.
        checkpoint(&mut interp)?;
        run_prologue(&mut interp, &guard, &validation_module, &signers, &txn_data)?;

        // User payload. Failure rolls back to the post-prologue state and
        // still charges gas via the failure epilogue.
        checkpoint(&mut interp)?;
        let mut gas = TxnGas::new(txn_data.max_gas_amount);
        let payload_result = self.execute_payload(&mut interp, &guard, &module_provider, txn, &gas);
        gas.record_payload(interp.gas_balance());

        let vm_status = match payload_result {
            Ok(()) => VMStatus::Executed,
            Err(status) => {
                rollback(&mut interp, 1)?;
                status
            },
        };

        // Epilogue (same function for success and failure). Its failure is an
        // invariant violation and discards the transaction.
        run_epilogue(
            &mut interp,
            &guard,
            &validation_module,
            &signers,
            &txn_data,
            gas.fee_statement(),
            gas.remaining(),
        )?;

        // Publication: drain writes and events while the heap is still alive.
        let internal = |e: anyhow::Error| {
            VMStatus::error(
                StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR,
                Some(format!("output assembly failed: {e:#}")),
            )
        };
        let writes = drain_write_set(&interp, &guard, &resource_provider).map_err(internal)?;
        let events = finalize_events(interp.extensions(), &guard).map_err(internal)?;

        let status = TransactionStatus::from_vm_status(vm_status.clone(), &features, true);
        let output = into_vm_output(writes, events, gas.fee_statement(), status);
        Ok((vm_status, output))
    }

    /// Runs the transaction's payload. Only entry functions are supported.
    fn execute_payload(
        &self,
        interp: &mut Interp<'_, '_, '_>,
        guard: &ExecutionGuard<'_>,
        module_provider: &impl ModuleProvider,
        txn: &SignedTransaction,
        gas: &TxnGas,
    ) -> Result<(), VMStatus> {
        match txn.payload().executable_ref() {
            Ok(TransactionExecutableRef::EntryFunction(entry)) => {
                self.execute_entry_function(interp, guard, module_provider, txn, entry, gas)
            },
            // TODO(completeness): scripts, multisig payloads, encrypted
            // transactions, module publishing.
            Ok(TransactionExecutableRef::Script(_))
            | Ok(TransactionExecutableRef::Encrypted)
            | Ok(TransactionExecutableRef::Empty)
            | Err(_) => Err(VMStatus::error(
                StatusCode::FEATURE_UNDER_GATING,
                Some("aptos-vm-v2 only supports entry-function payloads".to_string()),
            )),
        }
    }

    fn execute_entry_function(
        &self,
        interp: &mut Interp<'_, '_, '_>,
        guard: &ExecutionGuard<'_>,
        module_provider: &impl ModuleProvider,
        txn: &SignedTransaction,
        entry: &EntryFunction,
        gas: &TxnGas,
    ) -> Result<(), VMStatus> {
        // TODO(completeness): entry-function validation — `entry` visibility,
        // no return values, allowed argument types, and constructed arguments
        // (`String`, `Object<T>`, `Option<..>`) from
        // `transaction_arg_validation`.
        let module = fetch_module(
            module_provider,
            &entry.module().address,
            entry.module().name().as_str(),
        )
        .map_err(|e| VMStatus::error(StatusCode::LINKER_ERROR, Some(format!("{e:#}"))))?;

        let interned_ty_args = entry
            .ty_args()
            .iter()
            .map(|tag| types::intern_type_tag(guard, tag))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| VMStatus::error(StatusCode::INVALID_SIGNATURE, Some(format!("{e:#}"))))?;
        let ty_args = guard.type_list_of(&interned_ty_args);

        // Signers: the sender plus any secondary signers.
        // TODO(completeness): multi-agent transactions are untested.
        let mut signer_bufs = vec![txn.sender().into_bytes()];
        for secondary in txn.authenticator().secondary_signer_addresses() {
            signer_bufs.push(secondary.into_bytes());
        }

        let outcome = call_function(
            interp,
            guard,
            &module,
            &entry.module().address,
            entry.module().name(),
            entry.function(),
            ty_args,
            &signer_bufs,
            entry.args(),
            gas.payload_budget(),
        )
        .map_err(runtime_error_to_status)?;

        match outcome {
            CallOutcome::Success => Ok(()),
            CallOutcome::Abort { code, message } => Err(payload_abort_status(code, message)),
            CallOutcome::Error(err) => Err(runtime_error_to_status(err)),
        }
    }
}

impl Default for AptosVMv2 {
    fn default() -> Self {
        Self::new()
    }
}

/// Fetches and deserializes a module through the provider.
fn fetch_module(
    provider: &impl ModuleProvider,
    address: &AccountAddress,
    name: &str,
) -> anyhow::Result<CompiledModule> {
    let bytes: Bytes = provider
        .get_module_bytes(address, name)?
        .ok_or_else(|| anyhow::anyhow!("module {address}::{name} not found"))?;
    provider.deserialize_module(&bytes)
}

fn checkpoint(interp: &mut Interp<'_, '_, '_>) -> Result<(), VMStatus> {
    interp.checkpoint().map_err(|e| {
        VMStatus::error(
            StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR,
            Some(format!("checkpoint failed: {e}")),
        )
    })
}

fn rollback(interp: &mut Interp<'_, '_, '_>, n: usize) -> Result<(), VMStatus> {
    interp.rollback(n).map_err(|e| {
        VMStatus::error(
            StatusCode::UNKNOWN_INVARIANT_VIOLATION_ERROR,
            Some(format!("rollback failed: {e}")),
        )
    })
}
