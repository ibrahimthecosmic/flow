---
name: fmt
description: Format all code in the repository. Run before opening a PR or committing changes.
user-invocable: true
allowed-tools: Bash(./x fmt)
---

# Format Code

Run the Deno code formatter:

```sh
./x fmt
```

If any files were changed, stage and report them.

## Prerequisites

`./x` is a `deno run` script and `deno` is not on PATH on this machine —
symlink the built binary first (formatting via dprint does not rebuild
anything, so a stale flow binary is fine):

```sh
ln -sf "$PWD/target/debug/flow" <scratch>/bin/deno   # then prepend to PATH
# equivalent: PATH=<scratch>/bin:$PATH ./target/debug/flow run -A tools/format.js
```

The formatter needs the `tests/util/std` submodule. If it fails with "Module not
found ... tests/util/std":

```sh
git submodule update --init --depth 1 tests/util/std
```

## Gotchas

- **A failed run still writes changes.** dprint formats files as it goes, so if
  it errors partway (e.g. on a syntax-invalid file), everything before the error
  was already reformatted. After fixing the cause, check `git status` for
  unwanted fixture churn and revert it.
- **Test fixtures are excluded, keep it that way.** Fixture trees (some
  intentionally malformed, some mirroring upstream edge-runtime byte-for-byte)
  live in the `excludes` list of `.dprint.json` — currently
  `edge/crates/base/test_cases` and `edge/crates/base/tests/fixture/testdata`.
  When adding a new fixture directory, add it there AND to the dlint excludes in
  `tools/lint.js` (see `/lint-js`).
- rustfmt runs via dprint's exec plugin with
  `imports_granularity=item, group_imports=StdExternalCrate` — don't hand-roll
  import ordering; just run fmt.
- Editions differ per crate (`ext_workers`/`edge/cli` = 2024, `base` = 2021);
  fmt handles this via each crate's Cargo.toml, nothing to do manually.
