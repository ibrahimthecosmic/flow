---
name: merge-deno-upstream
description: Merge a newer denoland/deno release into flow. Use when bumping flow's Deno base version (e.g. 2.9.0 -> 2.10.x) from upstream denoland/deno.
user-invocable: true
---

# Merge a new Deno version from denoland/deno

flow = **full Deno** (root crates: `cli/`, `runtime/`, `ext/*`, `libs/*`) + the
**edge layer** (`edge/*`, vendored from supabase/edge-runtime via
trex-runtime) + a **rusty_v8 fork** (V8 Locker binding) + the **`flow` binary**
(`edge/cli`).

Upstream Deno owns the root crates; the edge layer is what drifts when Deno's
internal APIs change. Read the memory files first:
`flow-runtime-architecture.md` and `flow-runtime-progress.md` (in this project's
`memory/`). They record every non-obvious decision and the API-drift map.

## Procedure

1. **Branch & fetch.** `git checkout -b flow-<newver>` from the current flow
   branch. Add upstream once:
   `git remote add upstream https://github.com/denoland/deno` (if missing).
   `git fetch upstream --tags`.

2. **Bring in the new Deno core.** The root crates (`cli/ runtime/ ext/ libs/`)
   should track upstream tag `v<newver>`. Easiest: merge the tag, then re-apply
   flow's root-crate changes. flow's deviations from stock Deno in the root are
   intentionally small — search them via
   `git log v2.9.0..HEAD -- cli/ libs/ ext/ runtime/`. Key ones:
   - `cli/lib.rs`: the **facade** — `pub use deno_*` re-exports,
     `pub use deno_lib/deno_runtime`, and `pub mod factory/module_loader/...`
     that the edge layer consumes as `deno::…`. Re-apply/extend after the merge.
   - `cli/Cargo.toml`: `deno_runtime = { features = ["transpile"] }` in BOTH
     `[dependencies]` and `[build-dependencies]`.
   - `cli/lib/standalone/binary.rs`: `#[derive(Default)]` on
     `SerializedWorkspaceResolver` (if still needed by edge).
   - `libs/eszip/v2.rs`: `pub`-ified internals consumed by edge's eszip fork.
   - `Cargo.toml`: workspace members for `edge/*` + `edge/cli`;
     `[patch.crates-io] v8 = { path = "../rusty_v8" }` + `trex_core` stub; edge
     third-party deps.
   - `.cargo/config.toml`: `RUSTY_V8_MIRROR` env.
   - **Lint/format infra carries flow changes — preserve on merge:**
     `tools/lint.js` (edge fixture excludes in dlint, `edge` in the top-level
     allowlist), `tools/copyright_checker.js` (`edge/**` exemption),
     `.dprint.json` (edge fixture excludes), `runtime/js/99_main.js` (flow ops
     in `NOT_IMPORTED_OPS`), `cli/lib.rs` (allows-with-reasons on the widened
     `pub mod` lines), `.gitignore` (`tests/sqlite_extension/target/`).

3. **Check the V8 version.** Each Deno tag pins a `v8` crate version
   (`Cargo.toml`). If it changed: in `../rusty_v8`, rebase the Locker binding
   onto upstream rusty_v8's matching tag, re-run the `konnecthub-build` workflow
   to publish the new mirror (gnu+musl static lib + src_binding), then bump the
   `v8` patch + `RUSTY_V8_MIRROR` tag here. See `flow-runtime-progress.md` for
   the mirror URL + asset naming.

4. **Re-port the edge-layer drift.** This is the real work. `edge/*` reaches
   into Deno internals through the `deno = { path = "./cli" }` facade.
   `cargo check -p deno_facade` then `-p base`, then `cargo check --workspace`.
   Group errors by ROOT CAUSE before editing (see
   `fix-root-causes-not-symptoms.md`). Historically the heavy drift is in:
   `deno_resolver::factory` (Workspace/Resolver factories), the npm resolver
   (`NpmResolver<TSys>`), eszip format, emit/tsconfig,
   `File`/`Metadata`/`SerializedWorkspaceResolver` structs.
   **`edge/crates/deno_facade/emitter.rs`'s `EmitterFactory` is a thin wrapper
   over `deno::factory::CliFactory`** — if CliFactory's API moved, fix it there;
   do NOT re-grow a hand-rolled factory.

5. **Hard constraints to preserve.**
   - **Dependency cycle:** `deno_facade` depends on the `cli` crate, so `cli`
     must NEVER depend on `deno_facade`/`base`. The `flow` binary (`edge/cli`)
     sits above both.
   - **No V8 init before `main`:** do NOT reintroduce a `#[ctor]` that calls
     `JsRuntime::init_platform`/`set_v8_flags` — it freezes V8 flags and makes
     `deno::main()` panic (`Check failed: !IsFrozen()`). The `flow` binary lets
     Deno own V8 init.

6. **Build & verify.** `cargo check --workspace` (green), then
   `cargo build -p flow --bin flow` (needs `libopenblas-dev`; uses `rust-lld` —
   GNU `ld` bus-errors on the multi-GB link). Smoke test:
   `./target/debug/flow --version`, `flow eval 'console.log(1)'`,
   `flow run x.ts`, `flow eszip bundle/unbundle` round-trip.

7. Run `/merge-edge-runtime` and `/merge-trex-runtime` afterwards if those
   upstreams also advanced. Commit per logical step; do not force-push.
