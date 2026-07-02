---
name: merge-edge-runtime
description: Bring security/feature/bugfix updates from supabase/edge-runtime into flow's edge layer. Use when porting upstream edge-runtime changes.
user-invocable: true
---

# Bring updates from supabase/edge-runtime

flow's `edge/*` tree is vendored from **supabase/edge-runtime** (originally via
the OHDSI/trex-runtime fork, which tracked Deno 2.7.14). flow has since ported
it to Deno 2.9.0+, dropped the auto HTTP server + main↔user-worker comms,
wrapped Deno's `CliFactory`, and rebranded `supabase`→`flow`. Read the
`flow-runtime-architecture.md` and `flow-runtime-progress.md` memory files
first.

## Directory mapping (upstream edge-runtime → flow)

| upstream edge-runtime                                                       | flow                                                               |
| --------------------------------------------------------------------------- | ------------------------------------------------------------------ |
| `crates/{base,base_rt,cpu_timer,deno_facade,eszip_trait,fs,http_utils,...}` | `edge/crates/<same>`                                               |
| `ext/{ai,env,event_worker,os,runtime,workers}`                              | `edge/ext/<same>`                                                  |
| `cli/`                                                                      | `edge/cli/` (flow's `flow` binary — heavily diverged)              |
| vendored `./deno` consolidated lib                                          | flow's own `cli/` crate via the `deno = { path = "./cli" }` facade |

## Procedure

1. **Get the upstream delta.** Clone/fetch supabase/edge-runtime to a scratch
   dir. Identify what changed since flow last synced (check
   `flow-runtime-progress.md` for the last-synced ref/date).
   `git log`/`git diff` the relevant `crates/*` and `ext/*` paths. Triage by the
   user's intent: security, bugfix, or feature.

2. **Per change, decide portability.** flow has diverged in big ways — map each
   incoming change onto flow's reality:
   - **Comms / HTTP server** (`crates/base/src/server.rs`,
     `UserWorkerMsgs::SendRequest`, request forwarding, `--ip`/`--port`): flow
     **dropped** these (comms redesign pending). Don't port the server/relay; do
     port worker-pool/supervisor/limit fixes that are independent of the
     transport.
   - **deno internals usage:** upstream targets an older Deno. Adapt API calls
     to flow's 2.9.0+ facade. The npm-resolver/emit/factory code in
     `deno_facade` is a **CliFactory wrapper** in flow — re-express upstream's
     hand-rolled factory logic as CliFactory delegation, don't copy it verbatim.
   - **eszip:** flow's eszip layer builds on `libs/eszip` (pub-ified). Keep the
     format compatible.

3. **Apply the supabase→flow rebrand to ported code.** New code from upstream
   will say `Supabase`/`SUPABASE`/`supabase` and `SUPABASE_*` env vars — rename
   to `Flow`/`FLOW`/`flow` / `FLOW_*`, EXCEPT genuine external references:
   - `@supabase/supabase-js` npm imports and `npm-supabase-*` fixture dirs (real
     npm package — keep).
   - URLs/comments pointing at the upstream `supabase/edge-runtime` repo (keep
     as provenance).

4. **Make ported code pass flow's lint policy** (upstream edge-runtime does NOT
   meet it): every `#[allow]` needs a `reason = "…"`, every unsafe block a
   `// SAFETY:` comment, no `eprintln!`/`println!` in runtime code (use `log::`
   macros), JS TODOs tagged `TODO(flow)`. New fixture dirs go into the
   `.dprint.json` + `tools/lint.js` excludes. Details: `/lint-all`, `/lint-js`.

5. **Build & verify** as in `/merge-deno-upstream` step 6, then run `/fmt` +
   `/lint-all`. Commit per change with a message noting the upstream source
   commit.

## Preserve these flow invariants

- `cli` must not depend on `deno_facade`/`base` (cycle).
- No `#[ctor]` initializing V8 before `main` (V8 flag-freeze panic).
- `flow` binary delegates non-`eszip` subcommands to `deno::main()`.
- The `trex` cargo feature was removed from `base` (its `trex_core` symbols
  don't exist in flow's stub) — don't reintroduce it when porting.
