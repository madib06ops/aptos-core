// Copyright (c) Aptos Foundation
// Licensed pursuant to the Innovation-Enabling Source Code License, available at https://github.com/aptos-labs/aptos-core/blob/main/LICENSE

//! Helpers for running Move functions inside the transaction's single
//! interpreter context: native registry construction, parameter
//! classification, and argument placement.

use anyhow::{anyhow, bail, Result};
use mono_move_core::{
    intern_sig_token,
    native::NativeName,
    types::{InternedType, InternedTypeList},
    Function, Interner, RuntimeInvariantViolation,
};
use mono_move_global_context::ExecutionGuard;
use mono_move_natives::{make_all_production_natives, Dispatch};
use mono_move_runtime::{
    InterpreterContext, ProductionContextFamily, ProductionNativeRegistry, RuntimeError,
    RuntimeStatus, TransactionContext,
};
use move_binary_format::{access::ModuleAccess, file_format::SignatureToken, CompiledModule};
use move_core_types::{account_address::AccountAddress, identifier::IdentStr};

pub(crate) type Interp<'i, 'guard, 'ctx> = InterpreterContext<'i, TransactionContext<'guard, 'ctx>>;

/// The production native registry keyed by interned names.
pub(crate) fn build_natives(guard: &ExecutionGuard<'_>) -> ProductionNativeRegistry {
    let mut natives = ProductionNativeRegistry::new();
    natives
        .register_all(
            make_all_production_natives::<ProductionContextFamily>()
                .into_iter()
                .map(|(addr, module, function, dispatch, func)| {
                    let module = guard.module_id_of(&addr, &module);
                    let function = guard.identifier_of(&function);
                    let name = match dispatch {
                        Dispatch::Polymorphic => NativeName::Polymorphic { module, function },
                        Dispatch::Monomorphic(ty_args) => NativeName::Monomorphic {
                            module,
                            function,
                            ty_args: guard.type_list_of(ty_args),
                        },
                    };
                    (name, func)
                }),
        )
        .expect("natives have unique qualified names");
    natives
}

/// How one parameter of a called function is filled into the root frame.
pub(crate) enum ParamKind {
    /// A `signer`/`&signer` parameter, filled from the signer list.
    Signer { by_ref: bool },
    /// Any other parameter, deserialized from BCS.
    Value { ty: InternedType },
}

/// Classifies the parameters of `function_name` in `module`.
pub(crate) fn classify_params(
    module: &CompiledModule,
    function_name: &IdentStr,
    guard: &ExecutionGuard,
    ty_args: InternedTypeList,
) -> Result<Vec<ParamKind>> {
    for def in module.function_defs() {
        let handle = module.function_handle_at(def.function);
        if module.identifier_at(handle.name) == function_name {
            let signature = module.signature_at(handle.parameters);
            return signature
                .0
                .iter()
                .map(|token| classify_token(guard, module, ty_args, token))
                .collect();
        }
    }
    bail!(
        "function {} not found in module {}",
        function_name,
        module.self_id()
    )
}

fn classify_token(
    guard: &ExecutionGuard,
    module: &CompiledModule,
    ty_args: InternedTypeList,
    token: &SignatureToken,
) -> Result<ParamKind> {
    use SignatureToken as S;
    Ok(match token {
        S::Signer => ParamKind::Signer { by_ref: false },
        S::Reference(inner) | S::MutableReference(inner) if matches!(**inner, S::Signer) => {
            ParamKind::Signer { by_ref: true }
        },
        other => ParamKind::Value {
            ty: guard.subst_type(intern_sig_token(other, module, guard)?, ty_args)?,
        },
    })
}

/// Fills the root frame of `func` from the signer and BCS-argument lists, in
/// parameter order. `signer_bufs` must outlive the run: `&signer` parameters
/// are fat pointers into it.
pub(crate) fn place_args(
    interp: &mut Interp<'_, '_, '_>,
    func: &Function,
    params: &[ParamKind],
    signer_bufs: &[[u8; AccountAddress::LENGTH]],
    args: &[Vec<u8>],
) -> Result<()> {
    if func.param_slots.len() != params.len() {
        bail!(
            "lowered function has {} parameter slots but the signature has {} parameters",
            func.param_slots.len(),
            params.len()
        );
    }
    let mut signers = signer_bufs.iter();
    let mut args = args.iter();
    for (slot, kind) in func.param_slots.iter().zip(params) {
        let offset = slot.offset.0;
        match kind {
            ParamKind::Signer { by_ref } => {
                let signer = signers
                    .next()
                    .ok_or_else(|| anyhow!("not enough signers for the function"))?;
                if *by_ref {
                    // A reference is a 16-byte (base, byte_offset) fat pointer at the
                    // signer buffer. The base is outside the VM heap, so the GC
                    // leaves it alone.
                    let mut fat = [0u8; 16];
                    fat[..8].copy_from_slice(&(signer.as_ptr() as u64).to_le_bytes());
                    interp.set_root_arg(offset, &fat);
                } else {
                    interp.set_root_arg(offset, signer);
                }
            },
            ParamKind::Value { ty } => {
                let arg = args
                    .next()
                    .ok_or_else(|| anyhow!("not enough arguments for the function"))?;
                // SAFETY: `offset`/`ty` come from this function's own signature, so
                // the slot is valid for the type's in-memory size.
                unsafe { interp.deserialize_root_arg(offset, *ty, arg) }.map_err(|e| {
                    anyhow!("failed to place argument at frame offset {}: {}", offset, e)
                })?;
            },
        }
    }
    if args.next().is_some() {
        bail!("too many arguments for the function");
    }
    Ok(())
}

/// The outcome of one function call within the transaction.
pub(crate) enum CallOutcome {
    Success,
    Abort { code: u64, message: Option<String> },
    Error(RuntimeError),
}

/// Loads `module::function<ty_args>`, places arguments, and runs it in the
/// transaction's interpreter context with the given gas budget.
pub(crate) fn call_function(
    interp: &mut Interp<'_, '_, '_>,
    guard: &ExecutionGuard<'_>,
    module: &CompiledModule,
    address: &AccountAddress,
    module_name: &IdentStr,
    function_name: &IdentStr,
    ty_args: InternedTypeList,
    signer_bufs: &[[u8; AccountAddress::LENGTH]],
    args: &[Vec<u8>],
    gas_budget: u64,
) -> Result<CallOutcome, RuntimeError> {
    let module_id = guard
        .intern_address_name(address, module_name)
        .into_global_arena_ptr();
    let function = guard
        .intern_identifier(function_name)
        .into_global_arena_ptr();
    let func_ptr = interp
        .load_function(module_id, function, ty_args)
        .map_err(RuntimeError::Loader)?;
    // SAFETY: the pointer lives in a LoadedModule arena kept alive by the guard.
    let func: &Function = unsafe { func_ptr.as_ref_unchecked() };

    let params =
        classify_params(module, function_name, guard, ty_args).map_err(invariant_violation)?;

    interp.invoke(func);
    interp.reset_gas(gas_budget);
    place_args(interp, func, &params, signer_bufs, args).map_err(invariant_violation)?;

    match interp.run() {
        Ok(RuntimeStatus::Success) => Ok(CallOutcome::Success),
        Ok(RuntimeStatus::Aborted { code, message }) => Ok(CallOutcome::Abort { code, message }),
        Err(err) => Ok(CallOutcome::Error(err)),
    }
}

fn invariant_violation(err: anyhow::Error) -> RuntimeError {
    RuntimeError::InvariantViolation(RuntimeInvariantViolation::Unreachable(err.to_string()))
}
