// Copyright (c) Aptos Foundation
// Licensed pursuant to the Innovation-Enabling Source Code License, available at https://github.com/aptos-labs/aptos-core/blob/main/LICENSE

//! Conversions between runtime type tags and MonoMove interned types.

use anyhow::{anyhow, bail, Result};
use mono_move_core::{
    types::{
        view_name, view_type, view_type_list, InternedType, Type, ADDRESS_TY, BOOL_TY, I128_TY,
        I16_TY, I256_TY, I32_TY, I64_TY, I8_TY, SIGNER_TY, U128_TY, U16_TY, U256_TY, U32_TY,
        U64_TY, U8_TY,
    },
    Interner,
};
use mono_move_global_context::ExecutionGuard;
use move_core_types::{
    identifier::Identifier,
    language_storage::{StructTag, TypeTag},
};

// TODO(cleanup): the intern direction duplicates a copy in replay-benchmark; hoist both
// into global-context.

/// Interns a runtime [`TypeTag`] into a MonoMove [`InternedType`].
pub(crate) fn intern_type_tag(guard: &ExecutionGuard, tag: &TypeTag) -> Result<InternedType> {
    Ok(match tag {
        TypeTag::Bool => BOOL_TY,
        TypeTag::U8 => U8_TY,
        TypeTag::U16 => U16_TY,
        TypeTag::U32 => U32_TY,
        TypeTag::U64 => U64_TY,
        TypeTag::U128 => U128_TY,
        TypeTag::U256 => U256_TY,
        TypeTag::I8 => I8_TY,
        TypeTag::I16 => I16_TY,
        TypeTag::I32 => I32_TY,
        TypeTag::I64 => I64_TY,
        TypeTag::I128 => I128_TY,
        TypeTag::I256 => I256_TY,
        TypeTag::Address => ADDRESS_TY,
        TypeTag::Signer => SIGNER_TY,
        TypeTag::Vector(elem) => guard.vector_of(intern_type_tag(guard, elem)?),
        TypeTag::Struct(struct_tag) => intern_struct_tag(guard, struct_tag)?,
        // TODO(completeness): function values.
        TypeTag::Function(_) => bail!("function type tags are not supported"),
    })
}

/// Interns a struct tag into its nominal type.
pub(crate) fn intern_struct_tag(
    guard: &ExecutionGuard,
    struct_tag: &StructTag,
) -> Result<InternedType> {
    let module_id = guard.module_id_of(&struct_tag.address, struct_tag.module.as_ident_str());
    let name = guard.identifier_of(struct_tag.name.as_ident_str());
    let args = struct_tag
        .type_args
        .iter()
        .map(|arg| intern_type_tag(guard, arg))
        .collect::<Result<Vec<_>>>()?;
    let ty_args = guard.type_list_of(&args);
    Ok(guard.nominal_of(module_id, name, ty_args))
}

/// Renders an [`InternedType`] back into a runtime [`TypeTag`], for storage
/// keys and event types. Fails on open or reference types, which cannot occur
/// as storage or event types.
pub(crate) fn type_tag_of(ty: InternedType) -> Result<TypeTag> {
    Ok(match view_type(ty) {
        Type::Bool => TypeTag::Bool,
        Type::U8 => TypeTag::U8,
        Type::U16 => TypeTag::U16,
        Type::U32 => TypeTag::U32,
        Type::U64 => TypeTag::U64,
        Type::U128 => TypeTag::U128,
        Type::U256 => TypeTag::U256,
        Type::I8 => TypeTag::I8,
        Type::I16 => TypeTag::I16,
        Type::I32 => TypeTag::I32,
        Type::I64 => TypeTag::I64,
        Type::I128 => TypeTag::I128,
        Type::I256 => TypeTag::I256,
        Type::Address => TypeTag::Address,
        Type::Signer => TypeTag::Signer,
        Type::Vector { elem } => TypeTag::Vector(Box::new(type_tag_of(*elem)?)),
        Type::Nominal { .. } => TypeTag::Struct(Box::new(struct_tag_of(ty)?)),
        // TODO(completeness): function values.
        Type::Function { .. } => bail!("function type tags are not supported"),
        Type::ImmutRef { .. } | Type::MutRef { .. } | Type::TypeParam { .. } => {
            bail!("reference or open type cannot be rendered as a type tag")
        },
    })
}

/// Renders a nominal [`InternedType`] as a [`StructTag`].
pub(crate) fn struct_tag_of(ty: InternedType) -> Result<StructTag> {
    let Type::Nominal {
        module_id,
        name,
        ty_args,
    } = view_type(ty)
    else {
        bail!("not a nominal type");
    };
    // SAFETY: interned module ids live in the global arena for the whole execution.
    let module = unsafe { module_id.as_ref_unchecked() };
    let type_args = view_type_list(*ty_args)
        .iter()
        .map(|arg| type_tag_of(*arg))
        .collect::<Result<Vec<_>>>()?;
    Ok(StructTag {
        address: *module.address(),
        module: identifier(view_name(module.name()))?,
        name: identifier(view_name(*name))?,
        type_args,
    })
}

fn identifier(name: &str) -> Result<Identifier> {
    Identifier::new(name).map_err(|e| anyhow!("invalid identifier {name:?}: {e}"))
}
