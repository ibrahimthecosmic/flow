---
name: merge-trex-runtime
description: Bring updates from the OHDSI/trex-runtime fork into flow. Use when porting trex-runtime changes (its edge layer and/or its deno-fork patches).
user-invocable: true
---

# Bring updates from OHDSI/trex-runtime

OHDSI/trex-runtime is a fork of supabase/edge-runtime that (a) carries its own
edge-layer changes and (b) ships a fork of Deno (`p-hoffmann/deno trex-v2.7.14`
= upstream Deno 2.7.14 + 10 commits / 54 files: os sandboxing, per-worker
console, worker threads, websocket fix, node-http fixes, dep/lockfile
reconcile). flow's `flow-2.9.0` already incorporated those 10 deno-fork commits
(commit `af822bec52`) and vendored trex's edge layer under `edge/*`. Read
`flow-runtime-architecture.md` + `flow-runtime-progress.md` first.

## Two kinds of trex change

1. **Edge-layer changes** (`crates/*`, `ext/*`): port like `/merge-edge-runtime`
   — map to `edge/crates/*` + `edge/ext/*`, adapt to flow's 2.9.0+ facade +
   CliFactory wrapper, apply the supabase→flow rebrand, skip the dropped
   server/comms.

2. **deno-fork patches** (changes trex made to _Deno itself_): these landed in
   flow's root crates (`cli/ runtime/ ext/ libs/`). To see flow's current trex
   deltas: `git show af822bec52` / `git log --grep=trex`. When trex advances its
   deno fork, diff its `deno/` against upstream Deno of the same version to
   isolate NEW patches, then re-apply them onto flow's (newer) Deno root crates,
   resolving drift. The `phoffmann` git remote (p-hoffmann/deno) + `trex-squash`
   branch reproduce the original step-4 cherry-pick.

## Procedure

1. Fetch OHDSI/trex-runtime (`develop`) and its `./deno` submodule/fork to a
   scratch dir. Determine what advanced since flow last synced (see
   `flow-runtime-progress.md`).
2. Separate the two change kinds above. Triage by user intent
   (security/bug/feature).
3. Port edge-layer changes via the `/merge-edge-runtime` rules.
4. Re-apply genuinely-new deno-fork patches onto flow's root crates; mind that
   flow may be on a newer Deno than trex (resolve API drift in flow's favor).
5. Apply the supabase→flow rebrand to ported code (keep external `@supabase/*` /
   repo-URL references).
6. Make ported code pass flow's lint policy (allow reasons, SAFETY comments, no
   eprintln debug prints, TODO(flow) tags) — see `/lint-all` + `/lint-js`.
7. Build & verify (`cargo check --workspace`; `cargo build -p flow`; smoke tests
   incl. `flow eszip` round-trip), then `/fmt` + `/lint-all`.

## Preserve these flow invariants

- `cli` must not depend on `deno_facade`/`base` (dependency cycle).
- No `#[ctor]` initializing V8 before `main` (V8 flag-freeze panic).
- `EmitterFactory` stays a thin wrapper over `deno::factory::CliFactory`.
- `flow` binary delegates non-`eszip` subcommands to `deno::main()`.
