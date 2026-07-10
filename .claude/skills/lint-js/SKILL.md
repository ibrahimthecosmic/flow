---
name: lint-js
description: Lint JS/TS code only. Use before opening a PR when only JavaScript or TypeScript files were changed (no Rust).
user-invocable: true
allowed-tools: Bash(./x lint-js)
---

# Lint JS/TS Code

Run the JS/TS linter (much faster than the full `./x lint`):

```sh
./x lint-js
```

Fix errors and re-run until clean. Needs the `tests/util/std` submodule
(`git submodule update --init --depth 1 tests/util/std`), and `deno` on PATH —
not installed on this machine; symlink the built binary first
(`ln -sf $PWD/target/debug/flow <scratch>/bin/deno`, prepend to PATH).

## File selection

dlint's file list lives in `tools/lint.js` (`dlint()` + git-pathspec excludes).
Fixture trees must be excluded there AND in `.dprint.json` — currently
`edge/crates/base/test_cases/**`, `edge/crates/{base,fs}/tests/fixture/**` (some
fixtures are intentionally invalid TS and crash the parser). A separate
`dlintPreferPrimordials()` pass covers `runtime/**` + `ext/**` JS.

## Conventions the rules enforce (flow code included)

- **ban-untagged-todo**: `// TODO: x` fails — tag flow-authored ones as
  `// TODO(flow): x`.
- **no-console**: runtime/ext JS may not use `console.*`; where console output
  IS the intent (worker error paths), add `// deno-lint-ignore no-console` on
  the preceding line.
- **no-unused-vars**: don't keep unused bindings for side-effect loads — write
  `core.loadExtScript("ext:...")` / `import "ext:..."` without a binding. Unused
  params → `_name`. Unused catch bindings → bare `catch {`.
- **camelcase**: JS bindings must be camelCase even when destructuring
  snake_case op names: `const { op_foo: opFoo } = core.ops` or assign
  `const opFoo = core.ops.op_foo`. (Direct
  `import { op_foo } from
  "ext:core/ops"` is exempt, but flow's main-isolate
  code can't use that — the snapshot freezes that module's export list.)
- **require-await**: async fn with no await → drop `async` if all return paths
  already yield promises; otherwise keep it with
  `// deno-lint-ignore require-await` + a comment for interface contracts.
- **prefer-primordials** (runtime/ext only): no bare `new Set(...)` / for-of
  over arrays etc. — use `SafeSet`, indexed loops, primordial wrappers, matching
  the surrounding file's imports from `ext:core/mod.js` primordials.
