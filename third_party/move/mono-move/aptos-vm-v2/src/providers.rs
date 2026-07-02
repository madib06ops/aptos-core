// Copyright (c) Aptos Foundation
// Licensed pursuant to the Innovation-Enabling Source Code License, available at https://github.com/aptos-labs/aptos-core/blob/main/LICENSE

//! Adapters serving MonoMove's module and resource reads from an Aptos
//! `StateView`: raw module bytes, and resources materialized from BCS into a
//! long-lived flat-value arena. Resource groups are split into members on
//! read; membership is remembered for write-set publication.

use crate::types::struct_tag_of;
use anyhow::{anyhow, bail, Result};
use aptos_framework::natives::code::PackageRegistry;
use aptos_types::{
    state_store::{state_key::StateKey, StateView},
    vm::module_metadata::{get_metadata, RuntimeModuleMetadataV1},
};
use bytes::Bytes;
use mono_move_core::{
    storage::{
        module_provider::ModuleProvider,
        resource_provider::{
            InMemoryStorageKey, ResourceProvider, ResourceProviderError, StorageRead,
        },
    },
    types::InternedType,
    FrameOffset, LayoutKind, LayoutProvider, ValueLayout, OBJECT_HEADER_SIZE,
};
use mono_move_global_context::ExecutionGuard;
use mono_move_runtime::{deserialize_into, Heap};
use move_binary_format::CompiledModule;
use move_core_types::{
    account_address::AccountAddress, identifier::Identifier, language_storage::StructTag,
    metadata::Metadata,
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    ptr::NonNull,
    str::FromStr,
};

/// Serves module bytes to the loader from a `StateView`.
pub struct StateViewModuleProvider<'s, S> {
    state_view: &'s S,
}

impl<'s, S: StateView> StateViewModuleProvider<'s, S> {
    pub fn new(state_view: &'s S) -> Self {
        Self { state_view }
    }
}

impl<S: StateView> ModuleProvider for StateViewModuleProvider<'_, S> {
    fn get_module_bytes(&self, address: &AccountAddress, name: &str) -> Result<Option<Bytes>> {
        let name = Identifier::from_str(name)?;
        let key = StateKey::module(address, &name);
        Ok(self
            .state_view
            .get_state_value(&key)
            .map_err(|e| anyhow!("module read failed: {e}"))?
            .map(|value| value.bytes().clone()))
    }

    fn deserialize_module(&self, bytes: &[u8]) -> Result<CompiledModule> {
        // TODO(correctness): use the on-chain deserializer config (max version, etc.).
        CompiledModule::deserialize(bytes).map_err(|e| anyhow!("deserialize failed: {e:?}"))
    }

    fn verify_module(&self, module: &CompiledModule) -> Result<()> {
        // TODO(correctness): use the on-chain verifier config instead of the default.
        move_bytecode_verifier::verify_module(module)
            .map_err(|e| anyhow!("verification failed: {e:?}"))
    }

    fn get_same_package_modules(
        &self,
        address: &AccountAddress,
        module_name: &str,
    ) -> Result<Vec<Identifier>> {
        let registry_tag =
            StructTag::from_str("0x1::code::PackageRegistry").expect("well-formed tag parses");
        let key = StateKey::resource(address, &registry_tag)
            .map_err(|e| anyhow!("bad state key: {e}"))?;
        let Some(value) = self
            .state_view
            .get_state_value(&key)
            .map_err(|e| anyhow!("package registry read failed: {e}"))?
        else {
            // No registry (e.g. genesis-internal code): the module is its own package.
            return Ok(vec![Identifier::from_str(module_name)?]);
        };
        let registry: PackageRegistry = bcs::from_bytes(value.bytes())?;
        for package in &registry.packages {
            let names = package
                .modules
                .iter()
                .map(|m| Identifier::from_str(&m.name))
                .collect::<Result<Vec<_>, _>>()?;
            if names.iter().any(|n| n.as_str() == module_name) {
                return Ok(names);
            }
        }
        bail!("module {address}::{module_name} not found in any package")
    }
}

/// How a resource key maps onto state storage: its own slot, or a member of a
/// resource group's slot.
enum ResourceLocation {
    Standalone(StateKey),
    GroupMember { group_key: StateKey, tag: StructTag },
}

/// Serves resource and table-item reads from a `StateView`, materializing each
/// value (BCS → flat) into a long-lived arena on first access. Also resolves
/// and remembers resource-group placement, which write-set publication reuses.
pub struct StateViewResourceProvider<'a, 'guard, 'ctx, S> {
    guard: &'guard ExecutionGuard<'ctx>,
    state_view: &'a S,
    inner: RefCell<ProviderState>,
}

/// The provider's interior-mutable state: caches and the value arena.
struct ProviderState {
    /// Long-lived arena holding materialized flat values. Must outlive the
    /// interpreter; never reset during the transaction.
    heap: Heap,
    materialized: HashMap<InMemoryStorageKey, StorageRead>,
    /// Resource-group membership per struct tag, resolved from the defining
    /// module's metadata. `None` = not a group member.
    group_membership: HashMap<StructTag, Option<StructTag>>,
    /// Pre-state content of each resource group read so far, keyed by the
    /// group's state key. Publication merges member writes back into these.
    group_blobs: HashMap<StateKey, BTreeMap<StructTag, Bytes>>,
}

impl<'a, 'guard, 'ctx, S: StateView> StateViewResourceProvider<'a, 'guard, 'ctx, S> {
    pub fn new(guard: &'guard ExecutionGuard<'ctx>, state_view: &'a S, arena_size: usize) -> Self {
        Self {
            guard,
            state_view,
            inner: RefCell::new(ProviderState {
                heap: Heap::new(arena_size),
                materialized: HashMap::new(),
                group_membership: HashMap::new(),
                group_blobs: HashMap::new(),
            }),
        }
    }

    /// The resource group a struct belongs to, if any. Cached per tag.
    pub fn group_of(&self, tag: &StructTag) -> Result<Option<StructTag>> {
        if let Some(group) = self.inner.borrow().group_membership.get(tag) {
            return Ok(group.clone());
        }
        let group = self.resolve_group_of(tag)?;
        self.inner
            .borrow_mut()
            .group_membership
            .insert(tag.clone(), group.clone());
        Ok(group)
    }

    /// Pre-state content of the group behind `group_key`, loading it if this
    /// is the first access. Missing group = empty map.
    pub fn group_blob(&self, group_key: &StateKey) -> Result<BTreeMap<StructTag, Bytes>> {
        if let Some(blob) = self.inner.borrow().group_blobs.get(group_key) {
            return Ok(blob.clone());
        }
        let members: BTreeMap<StructTag, Bytes> = match self
            .state_view
            .get_state_value(group_key)
            .map_err(|e| {
            anyhow!("group read failed: {e}")
        })? {
            Some(value) => bcs::from_bytes(value.bytes())?,
            None => BTreeMap::new(),
        };
        self.inner
            .borrow_mut()
            .group_blobs
            .insert(group_key.clone(), members.clone());
        Ok(members)
    }

    /// Where a resource lives in state storage. Fails only on unrenderable
    /// types or state-key construction errors.
    pub fn locate(&self, address: &AccountAddress, ty: InternedType) -> Result<ResourceFetch> {
        let tag = struct_tag_of(ty)?;
        let location = match self.group_of(&tag)? {
            Some(group_tag) => ResourceLocation::GroupMember {
                group_key: StateKey::resource_group(address, &group_tag),
                tag,
            },
            None => ResourceLocation::Standalone(
                StateKey::resource(address, &tag).map_err(|e| anyhow!("bad state key: {e}"))?,
            ),
        };
        Ok(ResourceFetch { location })
    }

    /// Resolves group membership from the defining module's Aptos metadata.
    fn resolve_group_of(&self, tag: &StructTag) -> Result<Option<StructTag>> {
        let key = StateKey::module(&tag.address, &tag.module);
        let Some(value) = self
            .state_view
            .get_state_value(&key)
            .map_err(|e| anyhow!("module read failed: {e}"))?
        else {
            return Ok(None);
        };
        let module = CompiledModule::deserialize(value.bytes())
            .map_err(|e| anyhow!("deserialize failed: {e:?}"))?;
        let Some(metadata) = aptos_metadata(&module.metadata) else {
            return Ok(None);
        };
        Ok(metadata
            .struct_attributes
            .get(tag.name.as_str())
            .into_iter()
            .flatten()
            .find_map(|attr| attr.get_resource_group_member()))
    }

    /// Raw pre-state BCS bytes for a key, going through the group blob for
    /// group members. `None` = does not exist.
    fn fetch_bytes(&self, key: &InMemoryStorageKey) -> Result<Option<Bytes>> {
        match key {
            InMemoryStorageKey::Resource { address, ty } => {
                match self.locate(address, *ty)?.location {
                    ResourceLocation::Standalone(state_key) => Ok(self
                        .state_view
                        .get_state_value(&state_key)
                        .map_err(|e| anyhow!("resource read failed: {e}"))?
                        .map(|value| value.bytes().clone())),
                    ResourceLocation::GroupMember { group_key, tag } => {
                        Ok(self.group_blob(&group_key)?.get(&tag).cloned())
                    },
                }
            },
            InMemoryStorageKey::TableItem { handle, key, .. } => {
                let state_key = StateKey::table_item(
                    &aptos_types::state_store::table::TableHandle(handle.address()),
                    key,
                );
                Ok(self
                    .state_view
                    .get_state_value(&state_key)
                    .map_err(|e| anyhow!("table item read failed: {e}"))?
                    .map(|value| value.bytes().clone()))
            },
        }
    }
}

/// A located resource: where its bytes live in state storage.
pub struct ResourceFetch {
    location: ResourceLocation,
}

impl ResourceFetch {
    /// The state key the resource is stored under (the group's key for group
    /// members) and, for group members, the member tag within the group.
    pub fn into_location(self) -> (StateKey, Option<StructTag>) {
        match self.location {
            ResourceLocation::Standalone(key) => (key, None),
            ResourceLocation::GroupMember { group_key, tag } => (group_key, Some(tag)),
        }
    }
}

impl<S: StateView> ResourceProvider for StateViewResourceProvider<'_, '_, '_, S> {
    fn get_resource(&self, key: &InMemoryStorageKey) -> Result<StorageRead, ResourceProviderError> {
        if let Some(&read) = self.inner.borrow().materialized.get(key) {
            return Ok(read);
        }

        let internal = |e: anyhow::Error| ResourceProviderError::InvariantViolation(e.to_string());
        let value_ty = match key {
            InMemoryStorageKey::Resource { ty, .. } => *ty,
            InMemoryStorageKey::TableItem { value_ty, .. } => *value_ty,
        };
        let read = match self.fetch_bytes(key).map_err(internal)? {
            None => StorageRead::DoesNotExist,
            Some(blob) => {
                let mut inner = self.inner.borrow_mut();
                let ptr = materialize_one(&mut inner.heap, self.guard, value_ty, &blob)
                    .ok_or_else(|| {
                        ResourceProviderError::InvariantViolation(format!(
                            "failed to materialize resource at {:?}",
                            key.address()
                        ))
                    })?;
                StorageRead::ExternalHeap { ptr, version: 0 }
            },
        };
        self.inner
            .borrow_mut()
            .materialized
            .insert(key.clone(), read);
        Ok(read)
    }
}

/// Materializes one value of type `ty` from its BCS `blob` into `heap`,
/// returning a pointer to the flat object. `None` if the type's layout is
/// unavailable or the arena is full.
fn materialize_one(
    heap: &mut Heap,
    guard: &ExecutionGuard,
    ty: InternedType,
    blob: &[u8],
) -> Option<NonNull<u8>> {
    let layout = guard.layout_by_ty(ty)?;
    let size = layout.size as usize;

    // Lowering already published the type's GC descriptor; these offsets are a
    // fallback that is ignored when it exists.
    let mut offsets = vec![];
    collect_pointer_offsets(guard, layout, 0, &mut offsets);
    let frame_offsets: Vec<FrameOffset> = offsets.into_iter().map(FrameOffset).collect();
    let descriptor = guard.publish_struct_descriptor(ty, layout.size, &frame_offsets);

    let obj = heap.alloc_object(OBJECT_HEADER_SIZE + size, descriptor)?;
    // SAFETY: `obj` is a freshly reserved object with `size` payload bytes;
    // `deserialize_into` writes the flat value there.
    unsafe { deserialize_into(guard, heap, ty, blob, obj.as_ptr()) }.ok()?;
    Some(obj)
}

/// Collects the byte offsets of heap-pointer slots within a value's payload,
/// matching the object descriptors MonoMove's lowering computes.
fn collect_pointer_offsets(
    guard: &ExecutionGuard,
    layout: &ValueLayout,
    base: u32,
    out: &mut Vec<u32>,
) {
    if layout.has_no_pointers_no_padding() {
        return;
    }
    match &layout.kind {
        LayoutKind::Vector { .. }
        | LayoutKind::Function
        | LayoutKind::Ref
        | LayoutKind::FrozenEnum { .. } => out.push(base),
        LayoutKind::Struct { fields } => {
            for field in fields.iter() {
                if let Some(sub) = guard.layout(field.id) {
                    collect_pointer_offsets(guard, sub, base + field.offset, out);
                }
            }
        },
        LayoutKind::Bool
        | LayoutKind::UnsignedInt
        | LayoutKind::SignedInt
        | LayoutKind::Address => {},
    }
}

/// Decodes the Aptos v1 runtime metadata from a module's metadata entries.
fn aptos_metadata(entries: &[Metadata]) -> Option<std::sync::Arc<RuntimeModuleMetadataV1>> {
    get_metadata(entries)
}
