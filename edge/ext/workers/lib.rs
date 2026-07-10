use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::Error;
use deno::deno_permissions::PermissionsOptions;
use deno_core::JsBuffer;
use deno_core::OpState;
use deno_core::Resource;
use deno_core::ResourceId;
use deno_core::SharedArrayBufferStore;
use deno_core::error::AnyError;
use deno_core::op2;
use deno_core::serde_json;
use deno_error::JsErrorBox;
use deno_facade::EszipPayloadKind;
use deno_telemetry::OtelConfig;
use deno_telemetry::OtelConsoleConfig;
use deno_telemetry::OtelPropagators;
use fs::http_fs::HttpFsConfigs;
use fs::s3_fs::S3FsConfigs;
use fs::tmp_fs::TmpFsConfig;
use once_cell::sync::Lazy;
use once_cell::sync::OnceCell;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::context::CreateUserWorkerResult;
use crate::context::UserWorkerMsgs;
use crate::context::UserWorkerRuntimeOpts;
use crate::context::WorkerContextInitOpts;

pub mod context;

use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use deno::deno_web::MessagePort;
use deno::deno_web::create_entangled_message_port;
use deno_core::TransferredResource;

/// flow: process-global registry that hands a freshly-created worker-side
/// `MessagePort` half from `op_user_worker_create` (main isolate) to the spawned
/// user worker. Keyed by a token rather than the worker UUID, because the UUID
/// is assigned inside the pool *after* the create message is sent, so the main
/// side can't key by it. The token travels in `UserWorkerRuntimeOpts`.
static PARENT_PORTS: Lazy<Mutex<HashMap<u64, MessagePort>>> =
  Lazy::new(|| Mutex::new(HashMap::new()));
static PORT_SEQ: AtomicU64 = AtomicU64::new(1);

/// flow: the ONE `CrossIsolateStore` shared by the flow main isolate and every
/// user-worker runtime. Backing transferable ArrayBuffers in structured clone:
/// `port.postMessage(data, [arrayBuffer])` detaches the buffer and hands its
/// backing store over via this registry, so raw bytes cross the main<->worker
/// boundary zero-copy (the raw-byte escape hatch of the MessagePort comms).
/// Both sides MUST resolve transfer ids against the same store; flow registers
/// this instance into the deno CLI side via
/// `deno::embed::register_shared_array_buffer_store`.
///
/// CAVEAT (shared-memory posture): the same store makes host-posted
/// `SharedArrayBuffer`s cloneable into a worker. Worker code cannot mint one
/// itself (user workers alias `SharedArrayBuffer` to `ArrayBuffer` and reject
/// shared WASM memory at bootstrap), so shared memory only exists when the
/// HOST explicitly posts an SAB — and such memory is not attributed to the
/// worker's memory limit. Host authors opt in per message.
pub static FLOW_SHARED_ARRAY_BUFFER_STORE: Lazy<SharedArrayBufferStore> =
  Lazy::new(Default::default);

/// Runtime default for a user worker's memory limit (MiB), used when
/// `create()` omits `memoryLimitMb`. flow resolves this from
/// `FLOW_USER_WORKER_MAX_HEAP_SIZE_MIB` at startup; when unset, the
/// compile-time `UserWorkerRuntimeOpts` default (512 MiB) applies. This is the
/// flow repurposing of edge's (inert here) main/event-worker heap env vars:
/// flow only ever spawns user workers, whose heap comes from `memory_limit_mb`.
pub static USER_WORKER_DEFAULT_MEMORY_LIMIT_MIB: OnceCell<u64> =
  OnceCell::new();

/// Address the shared user-worker inspector server is bound to, or unset when
/// the inspector is disabled. Set by flow at startup (from
/// `--user-worker-inspect` / `FLOW_USER_WORKER_INSPECTOR_ADDRESS`) when it
/// stands up the pool with an inspector. Consumed by [`op_user_worker_inspect`]
/// to build the DevTools URL for a specific worker.
pub static USER_WORKER_INSPECTOR_HOST: OnceCell<SocketAddr> = OnceCell::new();

/// The synthetic inspector target identifier for a user worker, keyed by its
/// unique pool `key`. Used as the "module URL" passed to the inspector server
/// on registration, so each worker gets a distinct DevTools target even when
/// several workers share the same `servicePath` (whose real module URL would
/// otherwise collide). Must be computed identically on both the worker side
/// (registration) and the main side ([`op_user_worker_inspect`]).
pub fn inspector_target_id(key: &Uuid) -> String {
  format!("flow://user-worker/{key}")
}

/// The DevTools WebSocket URL for the user worker identified by `key`, given the
/// inspector server `host`. Mirrors `InspectorInfo`'s uuid derivation
/// (`v5(NAMESPACE_URL, target_id)`) and URL format (`ws://host/ws/{uuid}`).
pub fn inspector_ws_url(host: &SocketAddr, key: &Uuid) -> String {
  let uuid =
    Uuid::new_v5(&Uuid::NAMESPACE_URL, inspector_target_id(key).as_bytes());
  format!("ws://{host}/ws/{uuid}")
}

/// Remove and return the worker-side `MessagePort` registered for `token`.
pub fn take_parent_port(token: u64) -> Option<MessagePort> {
  PARENT_PORTS.lock().unwrap().remove(&token)
}

/// flow: per-worker delivery channels for handing an ADDITIONAL parent-port
/// half to an already-running user worker. Used when the pool answers a
/// `create()` with an existing worker (`reused: true`): the fresh entangled
/// pair's worker half is never installed at boot (the worker already booted),
/// so it is pushed through this channel instead and surfaced in the worker as
/// a new parent port (SharedWorker-style extra connection). Registered by the
/// worker at boot, unregistered when its op_state drops.
static PORT_DELIVERY: Lazy<
  Mutex<HashMap<Uuid, mpsc::UnboundedSender<MessagePort>>>,
> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Main-isolate side: hand `port` to the running user worker identified by
/// `key`. Returns `false` (dropping the port) when the worker has no live
/// delivery channel, i.e. it already shut down or never registered one.
pub fn deliver_parent_port(key: &Uuid, port: MessagePort) -> bool {
  let delivery = PORT_DELIVERY.lock().unwrap();
  match delivery.get(key) {
    Some(tx) => tx.send(port).is_ok(),
    None => false,
  }
}

/// Worker-side receiver for parent ports delivered after boot (pool reuse),
/// plus the drop guard that unregisters the delivery channel when the worker
/// runtime (its op_state) is torn down. Installed at worker boot; polled by
/// `op_flow_recv_parent_port`.
pub struct FlowPortDelivery {
  key: Uuid,
  rx: Rc<tokio::sync::Mutex<mpsc::UnboundedReceiver<MessagePort>>>,
}

impl FlowPortDelivery {
  pub fn register(key: Uuid) -> Self {
    let (tx, rx) = mpsc::unbounded_channel();
    PORT_DELIVERY.lock().unwrap().insert(key, tx);
    Self {
      key,
      rx: Rc::new(tokio::sync::Mutex::new(rx)),
    }
  }
}

impl Drop for FlowPortDelivery {
  fn drop(&mut self) {
    PORT_DELIVERY.lock().unwrap().remove(&self.key);
  }
}

/// op_state marker holding the resource id of the worker's parent `MessagePort`
/// (installed during worker runtime build; read by `op_flow_parent_port_rid`).
pub struct FlowParentPortRid(pub ResourceId);

/// Register a `MessagePort` into `state`'s resource table, returning its rid.
/// Uses the public `TransferredResource` impl so no deno_web internals are
/// needed (the `MessagePortResource` fields are private).
pub fn add_message_port(state: &mut OpState, port: MessagePort) -> ResourceId {
  let resource: Rc<dyn Resource> = Box::new(port).receive();
  state.resource_table.add_rc_dyn(resource)
}

/// Worker-side: return the rid of this worker's parent `MessagePort`, if one was
/// installed (i.e. the worker was created via `FlowRuntime.userWorkers.create`).
#[op2(fast)]
pub fn op_flow_parent_port_rid(state: &mut OpState) -> i32 {
  state
    .try_borrow::<FlowParentPortRid>()
    .map(|it| it.0 as i32)
    .unwrap_or(-1)
}

/// Worker-side: await the next parent `MessagePort` delivered to this
/// already-running worker (a `create()` call the pool answered with
/// `reused: true`). Resolves to the new port's rid, or -1 when the delivery
/// channel is closed / was never registered (worker shutting down, or not a
/// flow user worker). Called in a loop from the worker bootstrap; the pending
/// promise is unref'd there so it never holds the event loop alive.
#[op2]
pub async fn op_flow_recv_parent_port(state: Rc<RefCell<OpState>>) -> i32 {
  let rx = {
    let op_state = state.borrow();
    match op_state.try_borrow::<FlowPortDelivery>() {
      Some(it) => it.rx.clone(),
      None => return -1,
    }
  };
  match rx.lock().await.recv().await {
    Some(port) => add_message_port(&mut state.borrow_mut(), port) as i32,
    None => -1,
  }
}

deno_core::extension!(
  // flow: the WORKER-side extension, registered into every user-worker
  // runtime. Ops-only: it carries the parent-`MessagePort` plumbing the worker
  // bootstrap consumes (bootstrap.js wires `FlowRuntime.parentPort[s]`).
  // Workers cannot create workers, so the create/cleanup/inspect ops are
  // host-only (below).
  user_workers,
  ops = [op_flow_parent_port_rid, op_flow_recv_parent_port,],
);

deno_core::extension!(
  // flow: the HOST-side extension, embedded into the flow main isolate on top
  // of Deno's CLI snapshot. It carries no ESM, because a freshly-added
  // ESM-bearing extension can't link against the snapshotted `ext:` modules
  // (deno_webidl/deno_web/...) and panics at init. The
  // `FlowRuntime.userWorkers` host surface is installed post-bootstrap by
  // calling these ops directly (see edge/cli/src/flow_main.js).
  user_workers_ops,
  ops = [
    op_user_worker_create,
    op_user_worker_cleanup_idle_workers,
    op_user_worker_inspect,
  ],
);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsOtelConfig {
  #[serde(default)]
  tracing_enabled: bool,
  #[serde(default)]
  metrics_enabled: bool,
  #[serde(default)]
  console: OtelConsoleConfig,
  #[serde(default)]
  propagators: HashSet<OtelPropagators>,
}

pub type JsonMap = serde_json::Map<String, serde_json::Value>;

#[derive(Deserialize, Serialize, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct UserWorkerCreateOptions {
  service_path: String,
  env_vars: Vec<(String, String)>,
  no_module_cache: bool,
  no_npm: Option<bool>,

  force_create: bool,
  allow_remote_modules: bool,
  custom_module_root: Option<String>,
  permissions: Option<JsPermissionsOptions>,

  maybe_eszip: Option<JsBuffer>,
  maybe_entrypoint: Option<String>,
  maybe_module_code: Option<String>,

  memory_limit_mb: Option<u64>,
  low_memory_multiplier: Option<u64>,
  worker_timeout_ms: Option<u64>,
  cpu_time_soft_limit_ms: Option<u64>,
  cpu_time_hard_limit_ms: Option<u64>,

  s3_fs_config: Option<S3FsConfigs>,
  tmp_fs_config: Option<TmpFsConfig>,
  http_fs: Option<HttpFsConfigs>,
  otel_config: Option<JsOtelConfig>,

  context: Option<JsonMap>,
  #[serde(default)]
  static_patterns: Vec<String>,
  #[serde(default)]
  allow_host_fs_access: Option<bool>,
}

/// It is identical to [`PermissionsOptions`], except for `prompt`.
#[derive(Clone, Debug, Eq, PartialEq, Default, Serialize, Deserialize)]
pub struct JsPermissionsOptions {
  pub allow_all: Option<bool>,
  pub allow_env: Option<Vec<String>>,
  pub deny_env: Option<Vec<String>>,
  pub allow_net: Option<Vec<String>>,
  pub deny_net: Option<Vec<String>>,
  pub allow_ffi: Option<Vec<String>>,
  pub deny_ffi: Option<Vec<String>>,
  pub allow_read: Option<Vec<String>>,
  pub deny_read: Option<Vec<String>>,
  pub allow_run: Option<Vec<String>>,
  pub deny_run: Option<Vec<String>>,
  pub allow_sys: Option<Vec<String>>,
  pub deny_sys: Option<Vec<String>>,
  pub allow_write: Option<Vec<String>>,
  pub deny_write: Option<Vec<String>>,
  pub allow_import: Option<Vec<String>>,
}

impl JsPermissionsOptions {
  fn into_permissions_options(self) -> PermissionsOptions {
    PermissionsOptions {
      prompt: false,
      allow_env: self.allow_env,
      deny_env: self.deny_env,
      ignore_env: None,
      allow_net: self.allow_net,
      deny_net: self.deny_net,
      allow_ffi: self.allow_ffi,
      deny_ffi: self.deny_ffi,
      allow_read: self.allow_read,
      deny_read: self.deny_read,
      ignore_read: None,
      allow_run: self.allow_run,
      deny_run: self.deny_run,
      allow_sys: self.allow_sys,
      deny_sys: self.deny_sys,
      allow_write: self.allow_write,
      deny_write: self.deny_write,
      allow_import: self.allow_import,
      deny_import: None,
    }
  }
}

#[op2]
#[serde]
pub async fn op_user_worker_create(
  state: Rc<RefCell<OpState>>,
  #[serde] opts: UserWorkerCreateOptions,
) -> Result<(String, bool, Option<u32>), JsErrorBox> {
  // flow: set up the duplex MessagePort channel for main<->worker comms. The
  // main half is registered in this (main) isolate and its rid returned to JS;
  // the worker half is stashed in the global registry under a token that
  // travels to the spawned worker via `UserWorkerRuntimeOpts`.
  let (main_port, worker_port) = create_entangled_message_port();
  let main_port_rid = add_message_port(&mut state.borrow_mut(), main_port);
  let parent_port_token = PORT_SEQ.fetch_add(1, Ordering::Relaxed);
  PARENT_PORTS
    .lock()
    .unwrap()
    .insert(parent_port_token, worker_port);

  let result_rx = {
    let op_state = state.borrow();
    let tx = op_state.borrow::<mpsc::UnboundedSender<UserWorkerMsgs>>();
    let (result_tx, result_rx) =
      oneshot::channel::<Result<CreateUserWorkerResult, Error>>();

    let UserWorkerCreateOptions {
      service_path,
      env_vars,
      no_module_cache,
      no_npm,

      force_create,
      allow_remote_modules,
      custom_module_root,
      permissions,

      maybe_eszip,
      maybe_entrypoint,
      maybe_module_code,

      memory_limit_mb,
      low_memory_multiplier,
      worker_timeout_ms,
      cpu_time_soft_limit_ms,
      cpu_time_hard_limit_ms,

      s3_fs_config: maybe_s3_fs_config,
      tmp_fs_config: maybe_tmp_fs_config,
      http_fs: maybe_http_fs_config,
      otel_config: maybe_otel_config,

      context,
      static_patterns,
      allow_host_fs_access,
    } = opts;

    let maybe_otel_config = maybe_otel_config.map(|it| OtelConfig {
      tracing_enabled: it.tracing_enabled,
      metrics_enabled: it.metrics_enabled,
      console: it.console,
      propagators: it.propagators,
      ..Default::default()
    });
    let user_worker_options = WorkerContextInitOpts {
      service_path: PathBuf::from(service_path),
      no_module_cache,
      no_npm,

      env_vars: env_vars.into_iter().collect(),
      conf: Box::new({
        static DEFAULT: Lazy<UserWorkerRuntimeOpts> =
          Lazy::new(Default::default);

        UserWorkerRuntimeOpts {
          memory_limit_mb: memory_limit_mb.unwrap_or_else(|| {
            USER_WORKER_DEFAULT_MEMORY_LIMIT_MIB
              .get()
              .copied()
              .unwrap_or(DEFAULT.memory_limit_mb)
          }),
          low_memory_multiplier: low_memory_multiplier
            .unwrap_or(DEFAULT.low_memory_multiplier),

          worker_timeout_ms: worker_timeout_ms
            .unwrap_or(DEFAULT.worker_timeout_ms),
          cpu_time_soft_limit_ms: cpu_time_soft_limit_ms
            .unwrap_or(DEFAULT.cpu_time_soft_limit_ms),

          cpu_time_hard_limit_ms: cpu_time_hard_limit_ms
            .unwrap_or(DEFAULT.cpu_time_hard_limit_ms),

          force_create,
          allow_remote_modules,
          custom_module_root,
          permissions: permissions
            .map(JsPermissionsOptions::into_permissions_options),

          context,
          allow_host_fs_access,
          maybe_parent_port_token: Some(parent_port_token),

          ..Default::default()
        }
      }),

      static_patterns,
      timing: None,

      maybe_eszip: maybe_eszip.map(EszipPayloadKind::JsBufferKind),
      maybe_module_code: maybe_module_code.map(String::into),
      maybe_entrypoint,

      maybe_s3_fs_config,
      maybe_tmp_fs_config,
      maybe_http_fs_config,
      maybe_otel_config,
    };

    tx.send(UserWorkerMsgs::Create(
      Box::new(user_worker_options),
      result_tx,
    ))
    .map(|_| result_rx)
    .map_err(|e| JsErrorBox::generic(format!("{:#}", e)))
  };

  // Any outcome that doesn't hand the fresh channel to a NEWLY BOOTED worker
  // must not strand it: drop the stashed worker half (a failed boot may have
  // already consumed it) and close the main half's resource.
  let close_main_port = |state: &Rc<RefCell<OpState>>| {
    let resource = state.borrow_mut().resource_table.take_any(main_port_rid);
    if let Ok(resource) = resource {
      resource.close();
    }
  };
  let close_channel = |state: &Rc<RefCell<OpState>>| {
    drop(take_parent_port(parent_port_token));
    close_main_port(state);
  };

  let result_rx = match result_rx {
    Ok(rx) => rx,
    Err(err) => {
      close_channel(&state);
      return Err(err);
    }
  };

  match result_rx.await {
    Err(err) => {
      close_channel(&state);
      Err(JsErrorBox::generic(
        AnyError::from(err)
          .context("failed to create worker")
          .to_string(),
      ))
    }

    Ok(Err(err)) => {
      close_channel(&state);
      Err(JsErrorBox::generic(format!("{err:#}")))
    }

    Ok(Ok(v)) => {
      if v.reused {
        // The pool answered with an ALREADY-RUNNING worker, so no boot picked
        // up the stashed worker half. Hand it to the live worker instead
        // (surfaces there as an extra parent port, SharedWorker-style). When
        // delivery fails (worker torn down between reuse decision and now),
        // close the channel and report a null port.
        let delivered = match take_parent_port(parent_port_token) {
          Some(port) => deliver_parent_port(&v.key, port),
          None => false,
        };
        if !delivered {
          close_main_port(&state);
          return Ok((v.key.to_string(), true, None));
        }
      }
      Ok((v.key.to_string(), v.reused, Some(main_port_rid)))
    }
  }
}

/// Return the DevTools WebSocket URL for the user worker identified by `key`
/// (the string returned by `op_user_worker_create`), or `None` when the
/// user-worker inspector is disabled or `key` is malformed. The URL is derived
/// purely from `key` + the configured inspector host, matching what the worker
/// registered under, so no cross-thread lookup is needed. flow exposes this as
/// `FlowRuntime.userWorkers`' `worker.inspect()`.
/// Returns the URL, or an empty string when the inspector is disabled or `key`
/// is malformed (deno_core ops can't return `Option<String>` directly).
#[op2]
#[string]
pub fn op_user_worker_inspect(#[string] key: String) -> String {
  let Some(host) = USER_WORKER_INSPECTOR_HOST.get() else {
    return String::new();
  };
  let Ok(key) = Uuid::parse_str(&key) else {
    return String::new();
  };
  inspector_ws_url(host, &key)
}

#[op2]
#[number]
pub async fn op_user_worker_cleanup_idle_workers(
  state: Rc<RefCell<OpState>>,
  #[number] timeout_ms: usize,
) -> usize {
  let msg_tx = {
    state
      .borrow()
      .borrow::<mpsc::UnboundedSender<UserWorkerMsgs>>()
      .clone()
  };

  let (tx, rx) = oneshot::channel();
  if msg_tx
    .send(UserWorkerMsgs::TryCleanupIdleWorkers(timeout_ms, tx))
    .is_err()
  {
    return 0;
  }

  (rx.await).unwrap_or_default()
}
