# aptos-vm-v2

Reimplementation of the AptosVM transaction-execution layer on the MonoMove VM.
Executes real Aptos user transactions single-threaded: prologue → payload →
epilogue → write-set publication, producing a `VMOutput`.

Proof-of-concept scope: entry functions only. Scripts, multisig, module
publishing, keyless/account-abstraction auth, orderless transactions, mempool
validation, and view functions are `TODO(completeness)` placeholders. Gas is a
placeholder budget (MonoMove's uncalibrated block costs against the
transaction's gas limit); IO/storage fees and refunds are `TODO(metering)`.

## Interface

- In: `SignedTransaction` (signature verification is the caller's job) + a
  `StateView`. `TODO(completeness)`: take Block-STM's per-transaction view
  traits (`ExecutorView`/`ResourceGroupView`) instead of raw `StateView`.
- Out: `(VMStatus, VMOutput)` — the same contract the legacy AptosVM produces.
- `AptosVMv2BlockExecutor` implements `VMBlockExecutor` with a sequential loop
  and owns the `GlobalContext` (code cache) across blocks.

## Modules

| Module | Purpose |
|---|---|
| `lib.rs` | `AptosVMv2` entry points and the transaction lifecycle driver |
| `providers.rs` | `StateView` → MonoMove `ModuleProvider`/`ResourceProvider`; BCS→flat materialization, resource-group split, table items |
| `session.rs` | Native registry, parameter classification, argument placement, running one function in the shared interpreter context |
| `metadata.rs` | The slice of transaction metadata the prologue needs |
| `validation.rs` | Versioned prologue/epilogue calls into `0x1::transaction_validation` |
| `gas.rs` | Placeholder gas policy |
| `publish.rs` | Minimal write-set drain: CoW'd entry = write, group merge-back, deterministic ordering |
| `convert.rs` | Event finalization and `VMChangeSet`/`VMOutput` assembly |
| `errors.rs` | MonoMove outcomes → `VMStatus`, prologue aborts → discard statuses |
| `types.rs` | `TypeTag` ↔ `InternedType` conversions |
| `block_executor.rs` | Sequential `VMBlockExecutor` impl over an output-overlay view |

## Key design decisions

- **One session, checkpoints.** No per-stage change sets, no respawned
  sessions, no view overlays: prologue, payload, and epilogue run in one
  interpreter context, a failed payload rolls the heap and read-write set back
  to the post-prologue checkpoint, and writes are drained exactly once at the
  end.
- **Only the current feature path.** The versioned prologue/epilogue is the
  only validation path; legacy variants and old gas versions are intentionally
  not ported.
- **Gas amounts diverge from the legacy VM** by design until MonoMove's
  schedule is calibrated; differential tests compare outputs with only the
  fee-embedding slots masked.
- `publish.rs` is a stopgap consumer of the runtime's read-write-set drain
  API; the real publication (modification detection, storage metadata,
  refunds) is a separate workstream that replaces it from inside the runtime.

## Testing

```bash
cargo test -p mono-move-aptos-vm-v2
```

`tests/e2e.rs` builds genesis state with `FakeExecutor` (dev-dependency), runs
the same transaction on the legacy VM and this crate, and compares status,
write sets, and events, masking only the gas-fee slots. Cases: a successful
p2p transfer, and an insufficient-balance abort (payload rollback + failure
epilogue).
