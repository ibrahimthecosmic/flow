# CI Option A — Lean tag pipeline (comment-out, don't delete)

Status: **planned, not yet implemented**

## Background

`ci.yml` in this fork is generated from `.github/workflows/ci.ts`. Its **only
trigger is `push: tags: ["v*"]`** (`ci.ts`
`createWorkflow({ on: { push: {
tags: ["v*"] } } })`). PRs and pushes to `main`
do **not** run it — the only PR-triggered workflow is `pr.yml`, which just lints
the PR title. All the `isPr` / `ci-full` / sharding / draft-skip logic in
`ci.ts` is dormant code inherited from upstream denoland/deno (where CI runs on
`main` + PRs).

Two consequences of the tags-only trigger, both currently costing us:

1. **The debug build is wasted.** `build debug linux-x86_64` compiles the whole
   workspace in debug and uploads `flow`/`denort`/`test_server` artifacts, but
   every test job's step list is guarded by `isNotTag`
   (`!startsWith(github.ref, 'refs/tags/')`), so on a tag push `test-debug`,
   `test-release`, `wpt`, `deno-core-test`, and `deno-core-miri` **skip every
   step**. Nothing consumes the debug artifacts. The debug build's only real
   value (catching `debug_assert!` failures, faster compile→test than release)
   only materialises if the debug _tests_ run — and they don't.

2. **The cache never restores or saves.** In `ci.generated.yml` the guards are:

   | Cache                     | Restore guard    | Save guard      |
   | ------------------------- | ---------------- | --------------- |
   | cargo home                | `!tags`          | `main && !tags` |
   | build output (`./target`) | `!main && !tags` | `main && !tags` |

   On a tag push `!tags` is false and `main` is false, so **every restore and
   every save is skipped**. Save is gated to `main`, which never triggers CI, so
   caches are never even written. Every tag build is a fully cold, from-scratch
   build of the entire Deno workspace, done **twice** (debug + release, each
   with LTO). That is the ~40 min.

   (V8 is already fetched prebuilt from the rusty_v8 mirror, so it is _not_
   compiled from source — that big win is already in place and must stay.)

## Goal (Option A)

Keep the tag build as a lean **release-build-and-publish** pipeline, roughly
halve its wall time, and stop spinning empty no-op jobs — **without deleting
anything**. Everything we turn off is _commented out_ in `ci.ts` so it can be
restored (e.g. when we later add real PR/`main` CI — "Option B") by
uncommenting.

Net effect on a `v*` tag after this change:

- `build release linux-x86_64` → builds release + publishes GitHub release
  assets. **Kept.**
- `lint` → **kept** (cheap, useful as a release sanity gate).
- Caching → **fixed** so the second and later tags build incrementally instead
  of cold.
- Everything debug + all the no-op test/wpt/deno-core jobs → **commented out.**

Out of scope (this is the future "Option B", tracked separately): adding a
`pull_request` / `push: branches: [main]` trigger so tests actually run before
tagging and caches populate on `main`. When we do that, we uncomment what this
plan commented out.

## Changes (all in `.github/workflows/ci.ts`)

Line numbers are approximate — match on the surrounding code, not the number.

### 1. Comment out the debug build item

`buildItems` currently holds a release entry and a debug entry:

```ts
const buildItems = handleBuildItems([{
  ...Runners.linuxX86Xl,
  profile: "release",
  use_sysroot: true,
  wpt: isNotTag,
}, {
  ...Runners.linuxX86,
  profile: "debug",
  use_sysroot: true,
}]);
```

Comment out the debug object (leave a note pointing at this plan):

```ts
const buildItems = handleBuildItems([{
  ...Runners.linuxX86Xl,
  profile: "release",
  use_sysroot: true,
  wpt: isNotTag,
} // Option A (edge/plans/ci-option-a-lean-tag-pipeline.md): debug build disabled.
  // On the tags-only trigger its test consumers (all isNotTag-gated) never run,
  // so the debug artifacts are unused. Re-enable together with a PR/main trigger.
  // {
  //   ...Runners.linuxX86,
  //   profile: "debug",
  //   use_sysroot: true,
  // },
]);
```

Because `buildJobs = buildItems.map(...)` derives the per-item `build` / `test`
/ `wpt` / `test-libs` jobs, and both the top-level `jobs` array and
`ciStatusJob.needs` iterate `buildJobs`, dropping the debug item from this array
automatically removes **all** debug-derived jobs from generation _and_ from
`needs` — no other edit needed for those. `test-release` and `wpt-release` still
generate from the release item (they no-op on tags; see step 3 if we also want
them gone).

### 2. Fix caching so tag builds are incremental

In `createCargoCacheHomeStep` (the cargo-home cache):

```ts
// before
restoreCacheStep: steps.restoreCacheStep.if(isNotTag),
saveCacheStep: steps.saveCacheStep.if(isMainBranch.and(isNotTag)),
// after — restore always; also save on tags so the next tag restores it
restoreCacheStep: steps.restoreCacheStep, // (drop the isNotTag guard)
saveCacheStep: steps.saveCacheStep.if(isMainBranch.or(isTag)),
```

In `createCacheSteps` (the `./target` build-output cache):

```ts
// before
buildCacheSteps.restoreCacheStep.if(isMainBranch.not().and(isNotTag)),
...
buildCacheSteps.saveCacheStep.if(isMainBranch.and(isNotTag)),
// after
buildCacheSteps.restoreCacheStep, // restore always (prefix restore-keys picks latest)
...
buildCacheSteps.saveCacheStep.if(isMainBranch.or(isTag)),
```

Why this works: the save key is `<prefix>-${{ github.sha }}` and the restore
step uses `restore-keys: <prefix>-`, so an unguarded restore on a tag build
picks up the **most recent** `<prefix>-*` cache — i.e. the previous tag's
`./target` (including the ThinLTO cache under `target/release/lto-cache`) — and
cargo builds incrementally. The first tag after this lands is still cold; every
tag after is warm. Only the `build-release` job writes the `build-main` target
cache (the test jobs no-op on tags and never reach their save step), so there is
no cache-key contention.

Note: this is the one part that is _edited_, not commented out — the guards are
small conditionals, and the change is additive (`isMainBranch.or(isTag)` still
does the right thing if/when a `main` trigger is added for Option B).

### 3. (Optional, cosmetic) Comment out the standalone no-op jobs

`deno-core-test` and `deno-core-miri` are standalone jobs (not derived from
`buildItems`) whose entire step lists are `isNotTag`-gated, so on tags they spin
a runner and skip everything — near-zero compute but they clutter the Actions
run with empty green checks. Same for the release `test-*`/`wpt-*` jobs if we
want a truly minimal tag run.

If we want them gone, comment out in **three** places (they are referenced by
object, so all three must move together or generation breaks):

1. their `job(...)` definitions (`denoCoreTestJob`, `denoCoreMiriJob`),
2. their entries in the top-level `jobs: [...]` array of `createWorkflow`,
3. their entries in `ciStatusJob.needs`.

Leave a `// Option A: ...` comment at each site. This step is **optional** — the
compute win is already captured by step 1; step 3 is purely to keep the tag
run's UI clean. Recommendation: do step 1 + 2 now, defer step 3.

## Regenerate the workflow

`ci.generated.yml` is generated — do not hand-edit. After changing `ci.ts`:

```bash
deno run --check --allow-write=. --allow-read=. --lock=./tools/deno.lock.json \
  .github/workflows/ci.ts
# or, since it's executable with the right shebang:
./.github/workflows/ci.ts
```

Then run the formatter/linter per CLAUDE.md before committing:

```bash
./tools/format.js && ./tools/lint.js --js
```

## Verification (no CI run needed)

Inspect the regenerated `ci.generated.yml`:

- `grep -n "build-debug\|test-debug\|test-release\|wpt-release" ci.generated.yml`
  → no `build-debug` / `test-debug` job ids remain (step 1).
- The `build-release` job's `Restore cache build output` / `Cache build output`
  steps have an `if:` that is **true on tags** (contains `refs/tags/` on the
  save side, and no `!tags` exclusion on restore) (step 2).
- `deno-core-*` present or absent per the step-3 decision.
- `ci-status` `needs:` lists only jobs that still exist.

Because the trigger is unchanged (`push: tags: ["v*"]`), **committing and
pushing to `main` does not run CI** — nothing fires until the next `v*` tag.
That satisfies "commit & push, but don't trigger." Do **not** push a tag.

## Rollback

Uncomment the blocks and revert the two cache-guard edits (or just
`git revert`). Nothing was deleted.

## Expected impact

- Tag build wall time: roughly halved immediately (one full workspace build
  instead of two), then much lower again on the _second_ tag once the target
  cache is warm and cargo builds incrementally.
- Actions UI: no more empty green debug/test jobs on release tags.
- Correctness of releases: unchanged — the release binary is still built,
  binary-checked in the sysroot, and its assets published exactly as today.
