---
name: lint-all
description: Lint all code (Rust + JS/TS). Use before opening a PR when Rust code was changed.
user-invocable: true
---

# Lint All Code

Run the full linter (Rust + JS/TS):

```sh
./x lint
```

Fix errors and re-run until clean. A full pass takes ~10 min; use the iteration
strategy below instead of re-running blindly.

## On this machine: don't run the workspace clippy locally

The all-features workspace clippy is the exact build shape that has
OOM-crashed this box. Local pre-commit verification (agreed with the user,
2026-07-10): run `./x lint-js` in full, plus clippy scoped to the touched
crates with lint.js's deny flags, capped:

```sh
CARGO_PROFILE_DEV_DEBUG=line-tables-only cargo clippy -j 2 --all-targets --locked \
  -p base -p fs -p ext_runtime -p ext_workers -p base_rt -- \
  -D warnings --deny clippy::unused_async --deny clippy::print_stderr \
  --deny clippy::print_stdout --deny clippy::large_futures \
  --deny clippy::allow_attributes_without_reason
```

(Adjust the `-p` list to the crates you touched.) Leave the full workspace
pass to CI. Also: `deno` is not on PATH — `./x` and `tools/lint.js` shell out
to it; symlink the built binary first
(`ln -sf $PWD/target/debug/flow <scratch>/bin/deno`, prepend to PATH).

## Prerequisite

Needs the `tests/util/std` submodule:

```sh
git submodule update --init --depth 1 tests/util/std
```

## What one run actually checks (tools/lint.js)

1. **Workspace clippy** — `--workspace --all-features --exclude deno_core`.
2. **deno_core clippy** — separate invocation with specific features.
3. **dlint + prefer-primordials** on JS/TS (see `/lint-js`).
4. **Copyright headers** (`tools/copyright_checker.js`) — `edge/**` is exempt
   (edge-runtime lineage, not "the Deno authors"); everything else needs
   `// Copyright 2018-<year> the Deno authors. MIT license.`
5. **Cargo.lock freshness** (`--locked`) — if you changed any Cargo.toml, run
   `cargo update -p <crate> --offline` first or this check fails.
6. **disallowed-methods enforcement** — EVERY crate dir under `ext/` and `libs/`
   must have a `clippy.toml` with the required disallowed-methods list. New ext
   crate → copy `ext/webidl/clippy.toml`.
7. **Generated workflows** — if `Cargo.toml` changed, regenerate:
   `deno run --allow-write=. --allow-read=. --allow-net=jsr.io --lock=./tools/deno.lock.json .github/workflows/ci.ts`
8. **Top-level entries** — new root dirs must be added to the allowlist in
   `tools/lint.js` (`ensureNoNewTopLevelEntries`); `edge` is already there.

## Iteration strategy (saves hours)

- Run `./x lint` in the background and capture the FULL output to a file — never
  `| tail -40`; error details scroll away and clippy runs two commands whose
  output interleaves.
- Clippy stops dependent crates when one fails ("waiting for other jobs"), so
  errors surface in dependency order across runs. To iterate on one crate
  without the 10-min loop, reproduce lint.js's flags exactly:

  ```sh
  cargo clippy -p <crate> --all-targets --all-features -- \
    -D warnings --deny clippy::unused_async --deny clippy::print_stderr \
    --deny clippy::print_stdout --deny clippy::large_futures \
    --deny clippy::allow_attributes_without_reason
  ```

  (Workspace rustflags in `.cargo/config.toml` add `-D clippy::all`,
  `await_holding_refcell_ref`, `missing_safety_doc`,
  `undocumented_unsafe_blocks` automatically.)
- NEVER run `cargo clippy -p deno_core --all-features` — it flips rusty_v8 into
  a from-source V8 build. Use lint.js's feature list for deno_core.

## Repo lint policy (fix style, not suppression)

- Every `#[allow(...)]` needs `reason = "..."`. Every `unsafe` block/impl needs
  a `// SAFETY:` comment on the preceding line (typos like "SATEFY" fail the
  lint).
- `#[allow]` directly on a macro-invocation statement (`println!(...)`) is
  silently IGNORED by rustc — wrap the statement in a block `{ ... }` and put
  the attribute on the block.
- `print_stdout`/`print_stderr` are denied: use `log::` macros in runtime code;
  genuine CLI output (flow's main.rs, flag-parse warnings in flow_config.rs)
  carries scoped allows with reasons; custom test harnesses use a file-level
  allow.
- `large_futures`: `Box::pin(...)` real code paths (e.g. `DenoRuntime::new`);
  test modules may carry a module-level allow (the test runtime boxes the root
  future).
- Prefer deleting dead code over `#[allow(dead_code)]`; keep the allow (with a
  reason saying why it's retained) only for pending-redesign seams.

## flow-specific mechanisms (why lints fire on unchanged upstream code)

- Upstream `cli/lib.rs` has PRIVATE modules; flow pubs many for the edge facade.
  That ARMS visibility-dependent lints (`hidden_glob_reexports`,
  `new_without_default`, `async_fn_in_trait`) inside untouched upstream files.
  Policy: allow these AT THE WIDENING SITE in `cli/lib.rs` (attributes on the
  `pub mod` lines, with reasons) — never patch upstream module bodies.
- The `deno` lib compiles TWICE in workspace clippy: once as a primary package
  (all features) and once as `flow`'s dependency (default features). Code behind
  `cfg(not(feature = ...))` (e.g. `cli/lsp/trace.rs` stubs) gets linted in the
  second build even though upstream CI never sees it.
