# A Prophecy-Based Reference Model for the Move Prover

This note describes how the Move Prover represents mutable references using *prophecies* (in the
style of RustHorn and Creusot). This is the **default** reference model; the legacy `WriteBack`
(path-based) model remains available behind `--path-refs`. The two models coexist; by default every
prover unit test runs under both and is expected to produce the same result. The design covers both
verification and (separately) spec inference.

The motivation is twofold. The prophecy model is conceptually simpler — it eliminates reference
*paths* and the runtime machinery that maintains them. And it is strictly more expressive: it can
naturally express *free* mutations (a `&mut` obtained through a function value, and prospectively one
stored in data) that the path-based model cannot.

> File and line references below are anchors against the tree at the time of writing and should be
> re-checked against current code.

# 1. Background: the `WriteBack` model

The default model represents a mutable reference as a value that carries, besides the referent's
current value, a description of *where the reference points*, so a mutation can be propagated back to
its origin when the reference goes out of scope (`prelude.bpl`):

```boogie
datatype $Mutation<T> {
    $Mutation(l: $Location, p: Vec int, v: T)   // root location l, selection path p, current value v
}
```

Propagation is an explicit `WriteBack(node, edge)` instruction inserted by
`MemoryInstrumentationProcessor` after borrow analysis, reconstructing the parent value along the
path. Where a reference's parent is not statically unique, a runtime `IsParent` predicate selects the
right write-back target. This is *constructive*: it replays every write up a statically known borrow
path, and it collapses exactly where that path is not static. The clearest symptom is
`BorrowEdge::Invoke` (a borrow taken via a function value, of unknown structure), where the write-back
cannot compute an update and falls back to discarding the post-state (`bytecode_translator.rs`):

```rust
if matches!(edge, BorrowEdge::Invoke) {
    emitln!(writer, "call $t{} := $HavocMutation($t{});", idx, idx);
}
```

As a consequence `closures/closure_refs.move::update_a` (which mutates `&mut s.a` through a function
value) cannot be verified and is marked expected-fail (`TODO(#17904)`).

# 2. The prophecy model

A mutable reference `&mut T` is represented as a pair of two values of type `T`: the *current* value
`v`, and the *final* value `f` the referent will hold when the borrow ends. `f` is a *prophecy
variable* — unknown at creation, pinned at expiry. Under the flag, this is the entire datatype:

```boogie
datatype $Mutation<T> { $Mutation(v: T, f: T) }   // v = current, f = final (prophecy)
```

Three rules govern it:

- **Creation**, `let r = &mut x`: pick a fresh prophecy `f`, set `r = $Mutation(x, f)`, and *eagerly*
  set the lender `x := f`. This is sound because, while `r` is live, Move's uniqueness invariant makes
  `x` unobservable; the only obligation is that `x` ends up holding whatever the borrower leaves in
  `*r`.
- **Write**, `*r = w`: update only the current value, `r := $UpdateMutation(r, w)`; the prophecy is
  untouched.
- **Resolution**, when the borrow ends: `assume v == f`. Because the lender was already set to `f` at
  creation, pinning `f` here retroactively communicates the borrower's net effect back to the lender.

That single idea — make the final value a symbolic prophecy and install it into the lender at
creation — is what removes the need for a statically known borrow path. The rest of this section is a
consequence of it. Reborrows compose by *chaining* prophecies: a child gets a fresh prophecy, and the
parent's value is constrained in terms of the child's. Aliasing of two references is simply *sharing
the same `f` term*.

Worked example, `fun inc(x: &mut u64) { *x = *x + 1 }` then `let a = 5; inc(&mut a); a`:

```text
borrow:   a := f          and   x = $Mutation(5, f)
body:     inc's contract gives  f == old(v) + 1 == 6
result:   a == f == 6
```

The solver proves `a == 6` by first-order reasoning over the pair — no heap, no path, no aliasing
reasoning, no quantified frame conditions.

## 2.1 The encoding is path-free

`$Dereference` and `$UpdateMutation` keep their signatures, so `ReadRef`/`WriteRef` translate
unchanged:

```boogie
function {:inline} $Dereference<T>(ref: $Mutation T): T { ref->v }
function {:inline} $UpdateMutation<T>(m: $Mutation T, v: T): $Mutation T { $Mutation(v, m->f) }
```

All of the static model's location/path apparatus — `$Location`, `$ChildMutation`,
`$IsParentMutation`, `$GlobalLocationAddress`, and friends — is gated out under the flag and never
referenced. The structural information it used to *store* is instead consumed once, at creation, where
the lender, the field offset, and the dynamic index are all available as operands of the borrow
instruction:

```text
&mut x        (x a local)      r := $Mutation(x, f);                        x   := f
&mut s.field  (s a reference)  r := $Mutation(s->v->field, f);             s   := $UpdateMutation(s, s->v[field := f])
&mut v[i]                       r := $Mutation(ReadVec(v->v, i), f);        v   := $UpdateMutation(v, UpdateVec(v->v, i, f))
&mut T[addr]  (global)          r := $Mutation(ResourceValue(Mem,addr), f); Mem := $ResourceUpdate(Mem, addr, f)
```

`f` is a havoc'd Boogie local introduced at translation time; no extra stackless temp is allocated.

## 2.2 `IsParent` becomes a logical constraint

In the static model `IsParent` is a *runtime* test that picks, at a write-back point with several
candidate parents, which one to update. Under prophecies there is nothing to pick: the parent/child
relation is the *defining equation* between prophecy terms, asserted at the borrow site (the child's
`f` literally occurs inside the parent's value). A conditional borrow needs no runtime branch — each
branch asserts its own equation under its own path condition, and Boogie's native path-sensitivity
carries it through the single resolve at the merge. The `IsParent` operation and its prelude support
have no analog, which as a side effect *removes* branches from the generated Boogie.

## 2.3 Calls under a live global borrow: the re-pin

The eager update installs the prophecy into the resource memory at the borrow, so the memory slot
and the borrow are connected from creation on. An intervening call can sever that connection: a
callee that (transitively) invokes a function value declaring `modifies_of` over the borrowed
memory havocs the whole memory variable, prophecy included. The static model is immune — its
write-back lands *after* the call, clobbering whatever the havoc did at the borrowed address.

The prophecy counterpart is a *re-pin*: right after each call made while a global borrow is live,
the instrumentation restores the eager update, `mem := ResourceUpdate(mem, addr, child->f)`
(a `ProphecyRepin` marker carrying the child reference and the borrow address, saved
into a dedicated temp at the borrow site). This is semantically exact, not an approximation:
Move's borrow rules guarantee no non-aborting callee can change an exclusively borrowed slot —
the `acquires` discipline for static calls, the dispatch reentrancy check for function values. It
is emitted as an assignment rather than an assumption so that a callee spec that (impossibly)
constrains the borrowed slot is overridden, exactly as the static write-back would, instead of
becoming a false assumption. Re-pins are invisible to the global-invariant analysis: they restore
a value that was already installed, and invariant assertion stays deferred to the
`ProphecyCommitGlobal` marker.

Only single-origin children are re-pinned (every definition of the reference temp is a
`BorrowGlobal` of one memory), so the saved address always matches the reaching borrow on every
path. A mixed-origin reference (borrowing a global on one branch, a local on the other) keeps the
incomplete-but-sound unpinned slot across calls, as do havocs not modeled as calls (loop-head
memory havocs, which `loop_analysis` inserts after this processor runs).

# 3. Mapping onto the prover

A single boolean selects the model at three points. It defaults to the prophecy model; setting it
opts back into the legacy `WriteBack` model.

## 3.1 The switch

- `ProverOptions::path_refs` (`bytecode-pipeline/src/options.rs`) — `#[arg(long)]` auto-creates
  `--path-refs` and the TOML key `prover.path_refs`. When `false` (the default) the prophecy model is
  used; when `true` the legacy `WriteBack` model is used.
- `BoogieOptions::path_refs` (`boogie-backend/src/options.rs`) — a mirror set from `ProverOptions`
  in `cli.rs::post_process`, required because the prelude is rendered by Tera with `BoogieOptions` as
  the context (`{% if not options.path_refs %}` selects the prophecy `$Mutation`).

The Aptos `prove` CLI exposes the same flag (`aptos-move/framework/src/prover.rs`).

## 3.2 The processor chain

The pipeline is reused unchanged through `BorrowAnalysisProcessor`. The single change is to swap the
memory processor (`pipeline_factory.rs`):

```rust
if options.prophecy_refs && !options.inference {
    ProphecyInstrumentationProcessor::new()   // verification only; see §3.5
} else {
    MemoryInstrumentationProcessor::new()
}
```

`ProphecyInstrumentationProcessor` consumes the same `BorrowAnnotation` and emits four operations: a
`ProphecyBorrow(lender, edge)` at each borrow (the eager lender update), a `Resolve` at each dying
reference (`assume v == f`), a `ProphecyCommitGlobal` where a global borrow dies (the global-state
transition marker consumed by the global invariant analysis), and a `ProphecyRepin` after each call
made under a live global borrow (§2.3). `ProphecyBorrow` mirrors the shape of `WriteBack`, so variable
remapping and instantiation are reused; placement reuses `dying_nodes`, whose child-first order is
exactly the order in which references must resolve. `PackRef`/`PackRefDeep` are still emitted at the dying nodes so
`DataInvariantInstrumentationProcessor` is unchanged.

## 3.3 Inter-procedural boundary

The existing call shape is kept: each `&mut` parameter is an input plus a trailing implicit return,
and a call `f(x)` is rewritten `x := f(x)`. The threaded value is a `$Mutation(v, f)` whose prophecy
the caller chose at the borrow it passes in; the callee discharges the contract by its own in-body
`Resolve`s before `Ret`. For an **opaque** call, the parameter's effect is realized by resolving its
prophecy (`assume $t_i->v == $t_i->f`); the callee's assumed postconditions then constrain
`$Dereference($t_i)` exactly as before, so the `ensures_of`/`result_of` behavioral machinery is reused
unchanged.

## 3.4 Closures and free mutations

A closure that takes a `&mut` *parameter* needs no special handling: the function-value apply boundary
threads the `$Mutation` through `$UpdateMutation`, like any opaque call (§3.3). This includes the
closures produced by lambda-lifting inline higher-order functions (e.g. `for_each_mut` with `|&mut T|`).

A `&mut` *derived through* a function value — returned by the closure, `BorrowEdge::Invoke` — is **not
yet supported**. The prophecy treatment would constrain the dying reference's prophecy against the
closure's behavioral footprint (`ensures_of`/`result_of`), a value-level relation at the `(v, f)`
granularity. A naive over-approximation here is *unsound* — it lets the closure's effect be assumed
away, making any post-condition provable — so until it is implemented properly the translator emits a
clear error for this case rather than a result. This is the documented next step; it would unlock
`closure_refs.move::update_a`. It depends on the language ban (`closure_checker.rs`) that prevents a
closure from *capturing* a `&mut` into its environment: that ban keeps every `Invoke` edge
depth-increasing and so resolution acyclic (§5), and must not be relaxed.

## 3.5 Spec inference (independent tool)

Spec inference is a separate tool that happens to share bytecode infrastructure; it produces ordinary
Move specifications, which are *model-agnostic*. It therefore always runs against the static
instrumentation — the pipeline selects the prophecy processor only in verification mode
(`!path_refs && !inference`). This keeps the intricate weakest-precondition
machinery unchanged. The relevant property is that the *inferred specifications verify under the
prophecy model* for every supported borrow form. Running inference itself against the prophecy
instrumentation is a possible follow-up; it would not change the inferred specifications.

# 4. Coexistence and testing

The two models share one baseline. The `default` feature now runs the prophecy model, and the `path`
feature in `tests/testsuite.rs` (`flags: ["--path-refs"]`, `inclusion_mode: Implicit`,
`separate_baseline: false`, enabled in CI) runs *every* unit test under the legacy static model as
well, against the same `foo.exp` — that shared baseline is the cross-check that the two models agree.
A test whose static output legitimately differs (typically a different counterexample trace) escapes
the sharing with `// separate_baseline: path` (yielding `foo.path_exp`); a case the default prophecy
model cannot verify is excluded from the default with `// exclude_for: default`.

One constraint shapes this. **The Boogie prelude is rendered once per run**, so a single run cannot
mix prophecy and static lowerings: a function lowered to `WriteBack` would reference prelude functions
that the prophecy prelude gates out. A (default) prophecy run therefore requires *every* function it
translates to be expressible in the prophecy model. Currently exactly one test is excluded from the
default (`closures/closure_refs.move`, the `Invoke` case of §3.4); it runs under `--path-refs` only.

# 5. Soundness

Three conditions from the literature must hold; each is met structurally by the prover's existing
analyses.

- **Resolve exactly once.** A reference dies once in the live-variable lattice, so `dying_nodes`
  yields one resolution point per reference. The conditional case is audited so exactly one `Resolve`
  is reachable per dying reference along any path.
- **Acyclic resolution.** Reborrow chaining defines a parent's prophecy in terms of its child's.
  Borrow analysis produces a DAG (a child's level is its parent's plus one), so the dependency order is
  the child-first order `dying_nodes` produces. The only way a back-edge could arise is `Invoke`
  aliasing, which the `closure_checker` ban prevents.
- **No early resolution.** `assume v == f` before the last write would be unsound; `dying_nodes` is
  after the last use by the definition of liveness, so resolution is never placed ahead of a write.

# 6. Status

Covered by the default prophecy model: local-root, field-on-reference, vector-index, variant-field,
table, and global-root borrows; conditional reborrows and returned `&mut` parameters; the
inter-procedural boundary including opaque calls; and closures with `&mut` parameters, including those
from lambda-lifting inline higher-order functions. Under `--path-refs` (the legacy model) output is
byte-identical to the prover's previous default.

Not yet supported — each fails with a clear error rather than producing an unsound result:

- A `&mut` *derived through* a function value (`BorrowEdge::Invoke`, §3.4); implementing it would
  unlock `closure_refs::update_a`.
- A `&mut` held *across* a loop back-edge: the loop invariant references the borrowed local, which
  under the eager model holds the unconstrained prophecy at the loop header. Making this sound requires
  treating the loop header as a resolve/re-borrow point so the invariant sees the current value.

The Move framework verifies under the default prophecy model (move-stdlib, aptos-stdlib, and
aptos-framework prover tests run it), with one function excluded —
`jwks::remove_oidc_provider_for_next_epoch` — for a borderline solver timeout at full-package
monomorphization (see §7). The model's soundness has not been systematically audited.

# 7. Performance

Verification time was compared between the two models on the whole `aptos-framework` package, function
by function, with `aptos prove --benchmark`. That mode verifies each function in isolation
(`verify_scope = Only`), single-threaded, so timings are deterministic and free of the cross-function
solver-context effects discussed below. All 1641 framework functions were measured under each model;
both completed with **no failures and no timeouts**. One function,
`jwks::remove_oidc_provider_for_next_epoch`, is marked `pragma verify = false` for this comparison: it
is the single function whose solver time is borderline against the 40s ceiling *at full-package
monomorphization* (a `&mut` local of a vector-of-structs passed through an opaque call), and it would
otherwise add noise unrelated to per-function cost.

| Metric | `WriteBack` (static) | Prophecy | Ratio |
| --- | --- | --- | --- |
| Total solver time, 1641 fns | 525.7 s | 493.7 s | 0.94× |
| Net of the ~276 ms boogie-startup floor | 72.8 s | 40.8 s | 0.56× |
| Functions slower under prophecy (>50 ms) | — | 41 | |
| Functions faster under prophecy (>50 ms) | — | 147 | |
| Roughly unchanged | — | 1453 | |

Per function, the prophecy model is at parity or slightly faster: total solver time is lower, and 3.6×
as many functions get faster as slower. Its path-free encoding (no `IsParent` checks, no location/path
component on `$Mutation`) is genuinely leaner per query. No function is made meaningfully slower — the
largest regression is `base16::base16_utf8_to_vec_u8` at +202 ms; the largest improvement is
`big_ordered_map::next_key` at −394 ms.

**This measures per-function cost, not whole-package cost.** Because each function is verified in
isolation, the benchmark does not exercise the one place the prophecy model is more expensive: a
single Boogie/Z3 session over a *large, shared* monomorphization. There, prophecy's per-`&mut`
`assume current == final` accumulates in the shared context, and whole-package verification is modestly
slower (≈1.2× on `aptos-stdlib`, ≈2.1× on the much smaller `move-stdlib`). This shared-context cost,
not any per-function penalty, is also why `remove_oidc_provider_for_next_epoch` sits on the timeout
boundary at full-package scale — a borderline-VC effect that, under parallel solving, also affects the
static model on other functions.

Artifacts (regenerate the plot with
`cargo run -p move-prover-lab -- plot --out=<f>.svg --sort <a>.fun_data <b>.fun_data`; series are
labelled by file name and sorted by the first file):

- [`static_vs_prophecy.svg`](static_vs_prophecy.svg) — per-function solver time, static vs. prophecy,
  sorted by static time.
- [`static.fun_data`](static.fun_data), [`prophecy.fun_data`](prophecy.fun_data) — the raw
  `name  solve_ms  status` records the plot is built from.

# References

- M. Matsushita, T. Tsukada, N. Kobayashi. *RustHorn: CHC-based Verification for Rust Programs.* TOPLAS 2021.
- Y. Matsushita, X. Denis, J.-H. Jourdan, D. Dreyer. *RustHornBelt.* PLDI 2022.
- X. Denis, J.-H. Jourdan, C. Marché. *Creusot: a Foundry for the Deductive Verification of Rust Programs.* ICFEM 2022.
- See also [`../fun_values_note.md`](../fun_values_note.md) for the function-value (closure) semantics this model builds on for free mutations.
