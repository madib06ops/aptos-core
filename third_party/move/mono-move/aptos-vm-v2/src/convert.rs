// Copyright (c) Aptos Foundation
// Licensed pursuant to the Innovation-Enabling Source Code License, available at https://github.com/aptos-labs/aptos-core/blob/main/LICENSE

//! Assembles the transaction's `VMOutput`: events out of the event store,
//! writes out of publication, and the final status.

use crate::types::type_tag_of;
use anyhow::{anyhow, Result};
use aptos_types::{
    contract_event::ContractEvent, event::EventKey, fee_statement::FeeStatement,
    state_store::state_key::StateKey, transaction::TransactionStatus,
};
use aptos_vm_types::{
    abstract_write_op::AbstractResourceWriteOp, change_set::VMChangeSet,
    module_write_set::ModuleWriteSet, output::VMOutput,
};
use mono_move_core::native::NativeExtensions;
use mono_move_global_context::ExecutionGuard;
use mono_move_natives::{EventKind, EventStore};
use mono_move_runtime::serialize;
use std::collections::BTreeMap;

/// Serializes the events recorded in the event store, in emission order.
///
/// Must run while the interpreter's heap is alive: each entry's value is
/// serialized straight out of VM memory.
pub(crate) fn finalize_events(
    extensions: &NativeExtensions,
    guard: &ExecutionGuard<'_>,
) -> Result<Vec<ContractEvent>> {
    let store = extensions
        .get_mut::<EventStore>()
        .map_err(|e| anyhow!("event store unavailable: {e:?}"))?;
    store
        .entries()
        .iter()
        .map(|entry| {
            let type_tag = type_tag_of(entry.msg_ty)?;
            // SAFETY: the heap is live for the duration of output assembly.
            let blob = unsafe { serialize(guard, entry.msg_data.as_ptr(), entry.msg_ty) }
                .map_err(|e| anyhow!("event value failed to serialize: {e}"))?;
            Ok(match &entry.kind {
                EventKind::V2 => ContractEvent::new_v2(type_tag, blob)?,
                EventKind::V1 {
                    guid,
                    sequence_number,
                } => {
                    let key: EventKey = bcs::from_bytes(guid)?;
                    ContractEvent::new_v1(key, *sequence_number, type_tag, blob)?
                },
            })
        })
        .collect()
}

/// Builds the final `VMOutput` from published writes, events, and status.
pub(crate) fn into_vm_output(
    writes: BTreeMap<StateKey, AbstractResourceWriteOp>,
    events: Vec<ContractEvent>,
    fee_statement: FeeStatement,
    status: TransactionStatus,
) -> VMOutput {
    let change_set = VMChangeSet::new(
        writes,
        events.into_iter().map(|event| (event, None)).collect(),
        BTreeMap::new(),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    // TODO(completeness): module publishing writes.
    VMOutput::new(change_set, ModuleWriteSet::empty(), fee_statement, status)
}
