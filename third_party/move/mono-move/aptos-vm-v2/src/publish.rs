// Copyright (c) Aptos Foundation
// Licensed pursuant to the Innovation-Enabling Source Code License, available at https://github.com/aptos-labs/aptos-core/blob/main/LICENSE

//! Minimal write-set publication: drains the transaction's read-write set into
//! Aptos write ops, merging resource-group members back into their group slot.
//!
//! This is a stopgap consumer of the runtime's drain API; the real publication
//! (modification detection, storage metadata, refunds, IO gas) is a separate
//! workstream that will replace it.
//
// TODO(correctness): every copied-on-write entry is emitted as a write, even
// when the bytes are unchanged (over-approximation). The legacy VM behaves the
// same way today, but byte-compare against the read is the intended endgame.
// TODO(metering): writes are not charged IO gas or storage fees.

use crate::{providers::StateViewResourceProvider, session::Interp};
use anyhow::{anyhow, Result};
use aptos_types::{
    state_store::{state_key::StateKey, StateView},
    write_set::WriteOp,
};
use aptos_vm_types::abstract_write_op::AbstractResourceWriteOp;
use bytes::Bytes;
use mono_move_core::storage::resource_provider::{InMemoryStorageKey, StorageRead};
use mono_move_global_context::ExecutionGuard;
use mono_move_runtime::{serialize, StorageWrite};
use move_core_types::language_storage::StructTag;
use std::collections::{BTreeMap, HashMap};

/// One member's new state within a resource group: new bytes or removal.
type MemberOp = Option<Bytes>;

/// Drains the read-write set into storage-level write ops.
///
/// Must run while the interpreter (and its heap) is alive: `LocalHeap` entries
/// are serialized straight out of VM memory.
pub(crate) fn drain_write_set<S: StateView>(
    interp: &Interp<'_, '_, '_>,
    guard: &ExecutionGuard<'_>,
    provider: &StateViewResourceProvider<'_, '_, '_, S>,
) -> Result<BTreeMap<StateKey, AbstractResourceWriteOp>> {
    let mut writes = BTreeMap::new();
    let mut group_ops: HashMap<StateKey, BTreeMap<StructTag, MemberOp>> = HashMap::new();

    for (key, entry) in interp.read_write_set().entries() {
        let new_bytes = match entry.write {
            StorageWrite::NotModified => continue,
            StorageWrite::Deleted { .. } => None,
            StorageWrite::LocalHeap { ptr, .. } => {
                let ty = value_type(key);
                // SAFETY: the entry's pointer refers to a live object in the
                // interpreter's heap, kept alive until after this drain.
                let blob = unsafe { serialize(guard, ptr.as_ptr(), ty) }
                    .map_err(|e| anyhow!("failed to serialize written value: {e}"))?;
                Some(Bytes::from(blob))
            },
        };
        let existed = matches!(entry.read, StorageRead::ExternalHeap { .. });

        match key {
            InMemoryStorageKey::Resource { address, ty } => {
                let (state_key, member_tag) = provider.locate(address, *ty)?.into_location();
                match member_tag {
                    None => {
                        writes.insert(state_key, plain_op(existed, new_bytes)?);
                    },
                    Some(tag) => {
                        group_ops
                            .entry(state_key)
                            .or_default()
                            .insert(tag, new_bytes);
                    },
                }
            },
            InMemoryStorageKey::TableItem { handle, key, .. } => {
                let state_key = StateKey::table_item(
                    &aptos_types::state_store::table::TableHandle(handle.address()),
                    key,
                );
                writes.insert(state_key, plain_op(existed, new_bytes)?);
            },
        }
    }

    // Merge group-member ops into each group's pre-state blob and emit one
    // write per group.
    // TODO(completeness): emit granular `WriteResourceGroup` ops (with per-tag
    // inner ops) once Block-STM integration needs them; a combined blob is
    // correct for storage.
    for (group_key, member_ops) in group_ops {
        let mut members = provider.group_blob(&group_key)?;
        let existed = !members.is_empty();
        for (tag, op) in member_ops {
            match op {
                Some(bytes) => {
                    members.insert(tag, bytes);
                },
                None => {
                    members.remove(&tag);
                },
            }
        }
        let new_bytes = if members.is_empty() {
            None
        } else {
            Some(Bytes::from(bcs::to_bytes(&members)?))
        };
        writes.insert(group_key, plain_op(existed, new_bytes)?);
    }

    Ok(writes)
}

/// Builds the plain write op for a slot: creation/modification/deletion by
/// whether the slot existed before and exists after.
//
// TODO(correctness): ops carry no `StateValueMetadata` (slot deposits, refunds,
// creation time); the legacy VM's `WriteOpConverter` fills these.
fn plain_op(existed: bool, new_bytes: Option<Bytes>) -> Result<AbstractResourceWriteOp> {
    let op = match (existed, new_bytes) {
        (false, Some(bytes)) => WriteOp::legacy_creation(bytes),
        (true, Some(bytes)) => WriteOp::legacy_modification(bytes),
        (true, None) => WriteOp::legacy_deletion(),
        (false, None) => {
            return Err(anyhow!("deletion of a slot that never existed"));
        },
    };
    Ok(AbstractResourceWriteOp::Write(op))
}

/// The value type stored at a read-write-set key.
fn value_type(key: &InMemoryStorageKey) -> mono_move_core::types::InternedType {
    match key {
        InMemoryStorageKey::Resource { ty, .. } => *ty,
        InMemoryStorageKey::TableItem { value_ty, .. } => *value_ty,
    }
}
