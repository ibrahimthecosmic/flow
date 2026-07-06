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

**Repo topology** (see `flow-runtime-progress.md`): flow lives in the detached
public repo `ibrahimthecosmic/flow` (`origin`), with `upstream` = denoland/deno.
Two long-lived branches: **`deno`** = a pristine upstream mirror (only ever
fast-forwards to a release commit; never holds flow commits) and **`main`** =
the flow dev line. A Deno upgrade happens on an **`upgrade/<denover>`** branch
merged back into `main` when done.

**Versioning & builds:** flow ships from its OWN tags (`vX.Y.Z`) — major aligned
with Deno, minor/patch diverge (flow can ship fixes/features off Deno's release
schedule). CI **builds only on Flow tag pushes**; branch pushes (`main`,
`upgrade/*`, `deno`) do NOT build, so validate locally
(`cargo build -p flow
--bin flow` + smoke tests) before tagging. A Flow
fix/feature is developed on `main`, then tagged to release. A Deno upgrade ends
by merging the upgrade branch to `main`, tagging, and deleting the branch (step
8).

## Procedure

1. **Sync + branch (scripted).** Run `tools/sync_upstream.sh <newver> --upgrade`
   (e.g. `tools/sync_upstream.sh 2.10.1 --upgrade`). It adds the `upstream`
   remote if missing, fetches ONLY the `v<newver>` commit (via
   `git fetch --no-tags upstream refs/tags/v<newver>` into `FETCH_HEAD` — NO tag
   is created locally), fast-forwards `deno` to it and pushes it, then creates
   `upgrade/<newver>` off `main` and merges `deno` in. Conflicts are expected —
   that is the port work below.
   **NEVER fetch all upstream tags** (`git fetch upstream` without `--no-tags`,
   or `git fetch --tags`): denoland ships hundreds, they pollute the local tag
   namespace, a `v<x>` tag would collide with the `upgrade/<x>` branch, and they
   later leak onto `origin` (see step 8). Only ever fetch the single target
   commit by its `refs/tags/<tag>` refspec into `FETCH_HEAD`. If you must fetch
   manually, mirror the script exactly.

2. **Bring in the new Deno core.** The merge in step 1 brings the root crates
   (`cli/ runtime/ ext/ libs/`) from the `deno` mirror; flow's own root-crate
   deviations live on `main` and surface as conflicts. flow's deviations from
   stock Deno in the root are intentionally small — list them via
   `git log deno..main -- cli/ libs/ ext/ runtime/`. Key ones:
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
   upstreams also advanced.

8. **Land it.** Once the upgrade branch builds + smoke-tests clean locally:
   ```
   git switch main && git merge --no-ff upgrade/<denover>
   git tag v<flow-version>              # flow's own version; NOT necessarily <denover>
   git push origin main                 # branch push does NOT build
   git push origin refs/tags/v<flow-version>   # push ONLY this tag -> triggers the build
   git branch -d upgrade/<denover>      # (the upgrade branch is local-only; nothing to delete on origin)
   ```
   Commit per logical step; do not force-push `main`.

   **NEVER `git push origin main --tags` / `git push --tags`.** The local clone
   still carries Deno's ~434 inherited historical tags (`std/*`, `v0.*`, `v1.*`,
   `v2.0`–`v2.8.*`), but `origin` must hold ONLY flow release tags. `--tags`
   publishes all 434 of them AND — because GitHub Actions **suppresses
   tag-triggered workflows when more than 3 tags are pushed at once** — the flow
   build never even starts. Always push the single release tag by its full
   refspec, as above. If junk tags ever leak onto origin, delete them (batched
   `git push origin :refs/tags/<t>`, keeping only the `vX.Y.Z` releases) and then
   delete + re-push the release tag ALONE to fire the build. See
   `flow-release-and-install.md` and `flow-runtime-progress.md` (release
   gotchas).

**Commit identity:** author commits as the user
(`MD. Ibrahim <ibrahimthecosmic@gmail.com>`). NEVER add "Claude"/Anthropic as
author or a `Co-Authored-By` trailer, and never mention Claude in the message —
even though the harness default suggests one.
