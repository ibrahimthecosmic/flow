// Copyright 2018-2026 the Deno authors. MIT license.

//! Flow-specific embedding hook.
//!
//! Lets the `flow` binary inject extra `deno_core` extensions into Deno's main
//! worker WITHOUT changing Deno's public worker APIs — which keeps upstream
//! `denoland/deno` merges friction-free. `flow`'s `main()` registers a factory
//! (via [`register_main_worker_extensions`]) before calling `deno::main()`;
//! the worker factory consults [`build_main_worker_extensions`] when it
//! constructs the main worker.
//!
//! The ONLY drift this mechanism introduces into upstream worker code is a
//! single call site in `cli/worker.rs`. When this hook is unused (plain Deno),
//! [`build_main_worker_extensions`] returns an empty vec and behavior is
//! identical to upstream.

use std::ffi::OsString;
use std::sync::OnceLock;

use deno_core::Extension;
use deno_core::JsRuntime;
use deno_core::SharedArrayBufferStore;
use deno_core::error::CoreError;

/// Produces a fresh set of extensions for each main worker. Extensions capture
/// per-worker op state and are not `Clone`, so registration takes a factory
/// rather than a prebuilt list.
type ExtensionFactory = Box<dyn Fn() -> Vec<Extension> + Send + Sync>;

static MAIN_WORKER_EXTENSIONS: OnceLock<ExtensionFactory> = OnceLock::new();

/// Register the factory that produces the additive main-worker extensions
/// (the flow/edge layer). Intended to be called once, before `deno::main()`.
/// Subsequent calls are ignored.
pub fn register_main_worker_extensions(factory: ExtensionFactory) {
  let _ = MAIN_WORKER_EXTENSIONS.set(factory);
}

/// Build the registered additive main-worker extensions, or an empty vec when
/// none were registered (i.e. running as plain Deno).
pub fn build_main_worker_extensions() -> Vec<Extension> {
  match MAIN_WORKER_EXTENSIONS.get() {
    Some(factory) => factory(),
    None => Vec::new(),
  }
}

/// Whether additive main-worker extensions have been registered (i.e. running
/// as flow rather than plain Deno). Used to disable the `run`-subcommand
/// `skip_op_registration` snapshot optimization: skipping op registration
/// assumes every extension's ops are baked into the CLI snapshot, but flow's
/// additive extensions are not, so their ops must be registered at startup.
pub fn has_main_worker_extensions() -> bool {
  MAIN_WORKER_EXTENSIONS.get().is_some()
}

/// Produces the flow-specific help section injected into `flow --help`. Deno's
/// root help uses a fixed template (`DENO_HELP`) with no `{subcommands}` slot,
/// so flow subcommands (e.g. the `eszip` group, which flow intercepts in its
/// own `main()` before `deno::main()`) would otherwise be invisible. This seam
/// carries a preformatted command group that is rendered just before the
/// "Environment variables" block. Returns styled text; empty for plain Deno.
type HelpSectionFactory = Box<dyn Fn() -> String + Send + Sync>;

static HELP_SECTION: OnceLock<HelpSectionFactory> = OnceLock::new();

/// Register flow's extra `--help` command group. Call once before
/// `deno::main()`. Subsequent calls are ignored.
pub fn register_help_section(factory: HelpSectionFactory) {
  let _ = HELP_SECTION.set(factory);
}

/// The registered flow help section, or `None` when running as plain Deno.
pub fn help_section() -> Option<String> {
  HELP_SECTION.get().map(|factory| factory())
}

/// Filters the process argv before Deno's flag parser sees it. flow uses this
/// to pull its own top-level flags (e.g. `--policy`, `--max-parallelism`) out
/// of the command line — Deno's clap would otherwise reject them as unknown —
/// while stashing the parsed values for the post-bootstrap pool setup. The
/// filter receives the full argv (including arg0) and returns the argv with
/// flow's flags removed. Identity for plain Deno.
type ArgFilter = Box<dyn Fn(Vec<OsString>) -> Vec<OsString> + Send + Sync>;

static ARG_FILTER: OnceLock<ArgFilter> = OnceLock::new();

/// Register the flow argv filter. Call once before `deno::main()`. Subsequent
/// calls are ignored.
pub fn register_arg_filter(filter: ArgFilter) {
  let _ = ARG_FILTER.set(filter);
}

/// Apply the registered argv filter, or return `args` unchanged when none was
/// registered (i.e. running as plain Deno).
pub fn apply_arg_filter(args: Vec<OsString>) -> Vec<OsString> {
  match ARG_FILTER.get() {
    Some(filter) => filter(args),
    None => args,
  }
}

/// The `CrossIsolateStore` backing transferable ArrayBuffers (and cloneable
/// SharedArrayBuffers) in the main worker's structured-clone machinery. flow
/// registers ONE store that is shared with its user-worker runtimes, so
/// `postMessage(data, [arrayBuffer])` over the main<->worker `MessagePort`s
/// moves the backing store zero-copy across isolates (the raw-byte path).
/// When unregistered (plain Deno), each run gets a private fresh store,
/// matching upstream behavior.
static SHARED_ARRAY_BUFFER_STORE: OnceLock<SharedArrayBufferStore> =
  OnceLock::new();

/// Register the process-wide `SharedArrayBufferStore`. Call once before
/// `deno::main()`. Subsequent calls are ignored.
pub fn register_shared_array_buffer_store(store: SharedArrayBufferStore) {
  let _ = SHARED_ARRAY_BUFFER_STORE.set(store);
}

/// The registered store, or a fresh default when none was registered (i.e.
/// running as plain Deno).
pub fn shared_array_buffer_store() -> SharedArrayBufferStore {
  SHARED_ARRAY_BUFFER_STORE.get().cloned().unwrap_or_default()
}

/// Runs once on a freshly bootstrapped main worker, *after* Deno's
/// `bootstrapMainRuntime` has set up the global scope. flow uses this to
/// install its additive globals (`FlowRuntime`/`Flow`) on top of a fully
/// initialized Deno scope — running at extension-eval time would be too early
/// (`globalThis.Deno` is not built yet).
type PostBootstrapHook =
  Box<dyn Fn(&mut JsRuntime) -> Result<(), CoreError> + Send + Sync>;

static MAIN_WORKER_POST_BOOTSTRAP: OnceLock<PostBootstrapHook> =
  OnceLock::new();

/// Register the post-bootstrap step for the main worker. Intended to be called
/// once, before `deno::main()`. Subsequent calls are ignored.
pub fn register_main_worker_post_bootstrap(hook: PostBootstrapHook) {
  let _ = MAIN_WORKER_POST_BOOTSTRAP.set(hook);
}

/// Run the registered post-bootstrap step, or a no-op when none was registered
/// (i.e. running as plain Deno).
pub fn run_main_worker_post_bootstrap(
  js_runtime: &mut JsRuntime,
) -> Result<(), CoreError> {
  match MAIN_WORKER_POST_BOOTSTRAP.get() {
    Some(hook) => hook(js_runtime),
    None => Ok(()),
  }
}
