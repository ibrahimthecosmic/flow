use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use base_rt::DropToken;
use base_rt::RuntimeState;
use deno_core::OpState;
use deno_core::ResourceId;
use deno_core::op2;
use deno_core::v8;
use deno_error::JsErrorBox;
use futures::task::AtomicWaker;
use serde::Serialize;
use tracing::debug;
use tracing::debug_span;

pub mod cert;
pub mod external_memory;
pub mod ops;

pub use ops::bootstrap::runtime_bootstrap;
pub use ops::net::runtime_net;

// Custom error type for ext/runtime operations
#[derive(Debug, thiserror::Error, deno_error::JsError)]
pub enum RuntimeError {
  #[class(inherit)]
  #[error(transparent)]
  Resource(#[from] deno_core::error::ResourceError),
  #[class(inherit)]
  #[error("{0}")]
  Io(#[from] std::io::Error),
  #[class("Runtime")]
  #[error("{0}")]
  Runtime(String),
  #[class(inherit)]
  #[error(transparent)]
  Other(
    #[from]
    #[inherit]
    deno_error::JsErrorBox,
  ),
}

impl From<anyhow::Error> for RuntimeError {
  fn from(err: anyhow::Error) -> Self {
    RuntimeError::Other(deno_error::JsErrorBox::generic(err.to_string()))
  }
}

pub struct MemCheckWaker(Arc<AtomicWaker>);

impl From<Arc<AtomicWaker>> for MemCheckWaker {
  fn from(value: Arc<AtomicWaker>) -> Self {
    Self(value)
  }
}

/// Wasm linear memory total in bytes, populated by wasm_memory_tracker.js.
#[derive(Clone, Default)]
pub struct WasmMemoryTracker(pub Arc<AtomicU64>);

impl WasmMemoryTracker {
  pub fn bytes(&self) -> u64 {
    self.0.load(Ordering::Relaxed)
  }
}

/// Pool-level worker counters. (The per-isolate heap/request metric sources
/// from the edge-runtime lineage were removed together with the HTTP server;
/// only the worker-pool bookkeeping survives.)
#[derive(Debug, Default, Clone)]
pub struct SharedMetricSource {
  active_user_workers: Arc<AtomicUsize>,
  retired_user_workers: Arc<AtomicUsize>,
}

impl SharedMetricSource {
  pub fn incl_active_user_workers(&self) {
    self.active_user_workers.fetch_add(1, Ordering::Relaxed);
  }

  pub fn decl_active_user_workers(&self) {
    self.active_user_workers.fetch_sub(1, Ordering::Relaxed);
  }

  pub fn incl_retired_user_worker(&self) {
    self.retired_user_workers.fetch_add(1, Ordering::Relaxed);
  }

  pub fn reset(&self) {
    self.active_user_workers.store(0, Ordering::Relaxed);
    self.retired_user_workers.store(0, Ordering::Relaxed);
  }
}

/*
#[op2(fast)]
fn op_is_terminal(state: &mut OpState, rid: u32) -> Result<bool, JsErrorBox> {
    let handle = state.resource_table.get_handle(rid)?;
    Ok(handle.is_terminal())
}*/

#[op2(fast)]
fn op_is_runtime_init(state: &mut OpState) -> bool {
  state.borrow::<Arc<RuntimeState>>().is_init()
}

#[op2(fast)]
fn op_stdin_set_raw(
  _state: &mut OpState,
  _is_raw: bool,
  _cbreak: bool,
) -> Result<(), JsErrorBox> {
  Ok(())
}

#[op2(fast)]
fn op_console_size(
  _state: &mut OpState,
  #[buffer] _result: &mut [u32],
) -> Result<(), JsErrorBox> {
  Ok(())
}

#[op2(fast)]
fn op_schedule_mem_check(state: &mut OpState) -> Result<(), JsErrorBox> {
  if let Some(waker) = state.try_borrow::<MemCheckWaker>() {
    waker.0.wake();
  }

  Ok(())
}

#[op2(fast)]
fn op_set_wasm_memory_bytes(
  state: &mut OpState,
  #[number] bytes: u64,
) -> Result<(), JsErrorBox> {
  if let Some(tracker) = state.try_borrow::<WasmMemoryTracker>() {
    tracker.0.store(bytes, Ordering::Relaxed);
  }
  Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MemoryUsage {
  rss: usize,
  heap_total: usize,
  heap_used: usize,
  external: usize,
}

#[op2]
#[serde]
fn op_runtime_memory_usage(scope: &mut v8::PinScope<'_, '_>) -> MemoryUsage {
  let s = scope.get_heap_statistics();

  MemoryUsage {
    // NOTE: Hardcoded for security.
    rss: 0,
    heap_total: s.total_heap_size(),
    heap_used: s.used_heap_size(),
    external: s.external_memory(),
  }
}

#[op2]
#[string]
pub fn op_read_line_prompt(
  #[string] _prompt_text: &str,
  #[string] _default_value: &str,
) -> Result<Option<String>, JsErrorBox> {
  Ok(None)
}

// Removed: now provided by ext/os
// #[op2(fast)]
// fn op_set_exit_code(
//   _state: &mut OpState,
//   #[smi] _code: i32,
// ) -> Result<(), JsErrorBox> {
//   Ok(())
// }

#[op2(fast)]
fn op_set_raw(
  _state: &mut OpState,
  _rid: u32,
  _is_raw: bool,
  _cbreak: bool,
) -> Result<(), JsErrorBox> {
  Ok(())
}

// flow(2.9.0 node-compat): `node:module` (01_require.js) statically imports
// `op_napi_open` from `ext:core/ops`; without an op of that name the module
// fails to instantiate and `require()` of npm packages breaks entirely. The
// real op lives in `deno_napi`, which is deliberately NOT registered in flow
// workers: native Node addons are excluded from the sandbox (same posture as
// upstream edge-runtime). This stub satisfies the import and only errors if a
// `.node` addon is actually `require()`d. Arity mirrors the real op's JS call
// site (8 args).
#[op2]
fn op_napi_open<'scope>(
  _scope: &mut v8::PinScope<'scope, '_>,
  #[string] path: String,
  _global: v8::Local<'scope, v8::Value>,
  _create_buffer: v8::Local<'scope, v8::Value>,
  _report_error: v8::Local<'scope, v8::Value>,
  _async_hooks_init: v8::Local<'scope, v8::Value>,
  _async_hooks_before: v8::Local<'scope, v8::Value>,
  _async_hooks_after: v8::Local<'scope, v8::Value>,
  _async_hooks_destroy: v8::Local<'scope, v8::Value>,
) -> Result<v8::Local<'scope, v8::Value>, JsErrorBox> {
  Err(JsErrorBox::generic(format!(
    "Native Node addons (N-API) are not supported in flow user workers \
     (attempted to load {path})"
  )))
}

#[derive(Debug, Default, Clone)]
pub struct PromiseMetrics {
  init: Arc<AtomicUsize>,
  resolve: Arc<AtomicUsize>,
}

impl PromiseMetrics {
  pub fn get_init_count(&self) -> usize {
    self.init.load(Ordering::Acquire)
  }

  pub fn get_resolve_count(&self) -> usize {
    self.resolve.load(Ordering::Acquire)
  }

  pub fn have_all_promises_been_resolved(&self) -> bool {
    self.get_init_count() == self.get_resolve_count()
  }
}

#[op2(fast)]
fn op_tap_promise_metrics(state: &mut OpState, #[string] kind: &str) {
  let _span = debug_span!("op_tap_promise_metrics", kind).entered();
  let metrics = if state.has::<PromiseMetrics>() {
    state.borrow_mut::<PromiseMetrics>()
  } else {
    state.put(PromiseMetrics::default());
    state.borrow_mut()
  };

  match kind {
    "init" => {
      metrics.init.fetch_add(1, Ordering::Release);
    }

    "resolve" => {
      metrics.resolve.fetch_add(1, Ordering::Release);
    }

    _ => {}
  }

  debug!(?metrics);
}

#[op2(fast)]
fn op_cancel_drop_token(
  state: &mut OpState,
  #[smi] rid: ResourceId,
) -> Result<(), JsErrorBox> {
  let token = state
    .resource_table
    .get::<DropToken>(rid)
    .map_err(|e| JsErrorBox::generic(e.to_string()))?;

  token.0.cancel();
  Ok(())
}

#[op2]
#[serde]
pub fn op_bootstrap_unstable_args(_state: &mut OpState) -> Vec<String> {
  vec![]
}

// Stub: node:process polyfill links against this op at module-load time.
// Upstream defines it in runtime::ops::worker_host, which we replace.
#[op2(fast)]
pub fn op_current_thread_cpu_usage(#[buffer] out: &mut [f64]) {
  out[0] = 0.0;
  out[1] = 0.0;
}

deno_core::extension!(
  runtime,
  // flow: deno_core 2.9.0 resolves an extension's ESM imports of another
  // extension's modules (incl. `lazy_loaded_js` like `ext:deno_webidl/
  // 00_webidl.js`) only when the dependency is declared here. This list mirrors
  // the `deno_*` extensions whose modules `js/*.js` import (bootstrap.js,
  // navigator.js, denoOverrides.js, ...). Without it the user worker panics at
  // boot: "ext:deno_webidl/00_webidl.js was not passed as an extension module".
  // (The edge-internal `env` extension is eager ESM but imports nothing from
  // other extensions, so it is intentionally omitted.)
  deps = [
    os,
    deno_telemetry,
    deno_webidl,
    deno_web,
    deno_webgpu,
    deno_fetch,
    deno_websocket,
    deno_crypto,
    deno_net,
    deno_io,
    deno_fs,
    deno_cache,
    deno_node
  ],
  ops = [
    // op_is_terminal,
    op_is_runtime_init,
    op_stdin_set_raw,
    op_console_size,
    op_read_line_prompt,
    // op_set_exit_code, // Removed: now provided by ext/os
    op_schedule_mem_check,
    op_set_wasm_memory_bytes,
    op_runtime_memory_usage,
    op_set_raw,
    op_bootstrap_unstable_args,
    op_napi_open,
    op_tap_promise_metrics,
    op_cancel_drop_token,
    op_current_thread_cpu_usage,
  ],
  esm_entry_point = "ext:runtime/bootstrap.js",
  esm = [
    dir "js",
    "98_global_scope_shared.js",
    "async_hook.js",
    "bootstrap.js",
    "denoOverrides.js",
    "errors.js",
    "fieldUtils.js",
    "namespaces.js",
    "navigator.js",
    "permissions.js",
    "promises.js",
    "wasm_memory_tracker.js",
  ]
);
