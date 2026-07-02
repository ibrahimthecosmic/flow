use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;
use std::task::Context;
use std::task::Poll;

use anyhow::Error;
use context::SendRequestResult;
use deno::deno_http::HttpRequestReader;
use deno::deno_http::HttpStreamReadResource;
use deno::deno_permissions::PermissionsOptions;
use deno_core::AsyncRefCell;
use deno_core::AsyncResult;
use deno_core::BufView;
use deno_core::ByteString;
use deno_core::CancelFuture;
use deno_core::CancelHandle;
use deno_core::CancelTryFuture;
use deno_core::JsBuffer;
use deno_core::OpState;
use deno_core::RcRef;
use deno_core::Resource;
use deno_core::ResourceId;
use deno_core::SharedArrayBufferStore;
use deno_core::WriteOutcome;
use deno_core::error::AnyError;
use deno_core::futures::FutureExt;
use deno_core::futures::Stream;
use deno_core::futures::StreamExt;
use deno_core::futures::stream::Peekable;
use deno_core::op2;
use deno_core::serde_json;
use deno_error::JsErrorBox;
use deno_facade::EszipPayloadKind;
use deno_telemetry::OtelConfig;
use deno_telemetry::OtelConsoleConfig;
use deno_telemetry::OtelPropagators;
use errors::WorkerError;
use ext_runtime::conn_sync::ConnWatcher;
use fs::s3_fs::S3FsConfig;
use fs::tmp_fs::TmpFsConfig;
use http_utils::utils::get_upgrade_type;
use hyper_v014::Body;
use hyper_v014::Method;
use hyper_v014::Request;
use hyper_v014::body::HttpBody;
use hyper_v014::header::CONTENT_LENGTH;
use hyper_v014::header::HeaderName;
use hyper_v014::header::HeaderValue;
use hyper_v014::upgrade::OnUpgrade;
use log::error;
use once_cell::sync::Lazy;
use once_cell::sync::OnceCell;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::context::CreateUserWorkerResult;
use crate::context::UserWorkerMsgs;
use crate::context::UserWorkerRuntimeOpts;
use crate::context::WorkerContextInitOpts;
use crate::context::WorkerRuntimeOpts;

pub mod context;
pub mod errors;

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
/// installed (i.e. the worker was created via `EdgeRuntime.userWorkers.create`).
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
  user_workers,
  ops = [
    op_user_worker_create,
    op_user_worker_fetch_build,
    op_user_worker_fetch_send,
    op_user_worker_cleanup_idle_workers,
    op_flow_parent_port_rid,
    op_flow_recv_parent_port,
  ],
  esm_entry_point = "ext:user_workers/user_workers.js",
  esm = ["user_workers.js",]
);

deno_core::extension!(
  // flow: an OPS-ONLY variant of `user_workers`, for embedding into the flow
  // main isolate on top of Deno's CLI snapshot. It carries no ESM, because a
  // freshly-added ESM-bearing extension can't link against the snapshotted
  // `ext:` modules (deno_webidl/deno_web/...) and panics at init. The
  // `EdgeRuntime.userWorkers` host surface is installed post-bootstrap by
  // calling these ops directly (see edge/cli/src/flow_main.js).
  //
  // The HTTP request-passing fetch ops are intentionally omitted: that was the
  // old one-way comms, being replaced by the MessagePort transport.
  user_workers_ops,
  ops = [
    op_user_worker_create,
    op_user_worker_cleanup_idle_workers,
    op_user_worker_inspect,
  ],
);

#[derive(Deserialize, Serialize, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct JsxImportBaseConfig {
  default_specifier: Option<String>,
  module: String,
  base_url: String,
}

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

  s3_fs_config: Option<S3FsConfig>,
  tmp_fs_config: Option<TmpFsConfig>,
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
      conf: WorkerRuntimeOpts::UserWorker(Box::new({
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
      })),

      static_patterns,
      timing: None,

      maybe_eszip: maybe_eszip.map(EszipPayloadKind::JsBufferKind),
      maybe_module_code: maybe_module_code.map(String::into),
      maybe_entrypoint,

      maybe_s3_fs_config,
      maybe_tmp_fs_config,
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
/// `EdgeRuntime.userWorkers`' `worker.inspect()`.
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

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct UserWorkerRequest {
  method: ByteString,
  url: String,
  headers: Vec<(String, String)>,
  has_body: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserWorkerBuiltRequest {
  request_rid: ResourceId,
  request_body_rid: Option<ResourceId>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserWorkerResponse {
  status: u16,
  status_text: String,
  headers: Vec<(ByteString, ByteString)>,
  body_rid: ResourceId,
  size: Option<u64>,
}

struct UserWorkerRequestResource(Request<Body>);

impl Resource for UserWorkerRequestResource {
  fn name(&self) -> std::borrow::Cow<'_, str> {
    "userWorkerRequest".into()
  }
}

struct UserWorkerRequestBodyResource {
  body: AsyncRefCell<Option<mpsc::Sender<Result<bytes::Bytes, Error>>>>,
  cancel: CancelHandle,
}

impl Resource for UserWorkerRequestBodyResource {
  fn name(&self) -> std::borrow::Cow<'_, str> {
    "userWorkerRequestBody".into()
  }

  fn write(self: Rc<Self>, buf: BufView) -> AsyncResult<WriteOutcome> {
    Box::pin(async move {
      let bytes: bytes::Bytes = buf.to_vec().into();
      let nwritten = bytes.len();
      let body = RcRef::map(&self, |r| &r.body).borrow_mut().await;
      let body = (*body).as_ref();
      let cancel = RcRef::map(self, |r| &r.cancel);
      let body = body.ok_or(JsErrorBox::type_error(
        "request body receiver not connected (request closed)",
      ))?;

      body.send(Ok(bytes)).or_cancel(cancel).await?.map_err(|e| {
        JsErrorBox::type_error(format!(
          "request body receiver not connected ({})",
          e
        ))
      })?;

      Ok(WriteOutcome::Full { nwritten })
    })
  }

  fn write_error(
    self: Rc<Self>,
    error: &dyn deno_error::JsErrorClass,
  ) -> AsyncResult<()> {
    // Convert error to string before async block to avoid lifetime issues
    let error_string = format!("{}", error);
    async move {
      let body = RcRef::map(&self, |r| &r.body).borrow_mut().await;
      let body = (*body).as_ref();
      let cancel = RcRef::map(self, |r| &r.cancel);
      let body = body.ok_or(JsErrorBox::type_error(
        "request body receiver not connected (request closed)",
      ))?;
      // Convert to anyhow::Error
      let anyhow_error: Error = anyhow::anyhow!(error_string);
      body
        .send(Err(anyhow_error))
        .or_cancel(cancel)
        .await?
        .map_err(|e| JsErrorBox::generic(format!("{:#}", e)))?;
      Ok(())
    }
    .boxed_local()
  }

  fn shutdown(self: Rc<Self>) -> AsyncResult<()> {
    async move {
      let mut body = RcRef::map(&self, |r| &r.body).borrow_mut().await;
      body.take();
      Ok(())
    }
    .boxed_local()
  }

  fn close(self: Rc<Self>) {
    self.cancel.cancel();
  }
}

type BytesStream =
  Pin<Box<dyn Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin>>;

struct UserWorkerResponseBodyResource {
  reader: AsyncRefCell<Peekable<BytesStream>>,
  size: Option<u64>,
  req_end_tx: mpsc::UnboundedSender<()>,
  cancel: CancelHandle,
  conn_token: Option<CancellationToken>,
}

impl Resource for UserWorkerResponseBodyResource {
  fn name(&self) -> std::borrow::Cow<'_, str> {
    "userWorkerResponseBody".into()
  }

  fn read(self: Rc<Self>, limit: usize) -> AsyncResult<BufView> {
    Box::pin(async move {
      let reader = RcRef::map(&self, |r| &r.reader).borrow_mut().await;

      let fut = async move {
        let mut reader = Pin::new(reader);
        loop {
          match reader.as_mut().peek_mut().await {
            Some(Ok(chunk)) if !chunk.is_empty() => {
              let len = std::cmp::min(limit, chunk.len());
              let chunk = chunk.split_to(len);
              break Ok(chunk.into());
            }
            // This unwrap is safe because `peek_mut()` returned `Some`, and thus
            // currently has a peeked value that can be synchronously returned
            // from `next()`.
            //
            // The future returned from `next()` is always ready, so we can
            // safely call `await` on it without creating a race condition.
            Some(_) => match reader.as_mut().next().await.unwrap() {
              Ok(chunk) => assert!(chunk.is_empty()),
              Err(err) => break Err(JsErrorBox::type_error(err.to_string())),
            },
            None => break Ok(BufView::empty()),
          }
        }
      };

      let cancel_handle = RcRef::map(self, |r| &r.cancel);
      fut.try_or_cancel(cancel_handle).await
    })
  }

  fn close(self: Rc<Self>) {
    self.cancel.cancel();

    let _ = self.req_end_tx.send(());
    let Ok(this) = Rc::try_unwrap(self) else {
      return;
    };

    tokio::spawn(async move {
      if let Some(token) = this.conn_token {
        token.cancelled_owned().await;
      }
    });
  }

  fn size_hint(&self) -> (u64, Option<u64>) {
    (self.size.unwrap_or(0), self.size)
  }
}

#[op2]
#[serde]
pub fn op_user_worker_fetch_build(
  state: &mut OpState,
  #[serde] req: UserWorkerRequest,
) -> Result<UserWorkerBuiltRequest, JsErrorBox> {
  let method = Method::from_bytes(&req.method)
    .map_err(|e| JsErrorBox::generic(format!("{:#}", e)))?;

  let mut builder = Request::builder().uri(req.url).method(&method);
  let mut body = Body::empty();
  let mut request_body_rid = None;

  if req.has_body {
    let (tx, stream) = mpsc::channel(1);

    body = Body::wrap_stream(BodyStream(stream));
    request_body_rid =
      Some(state.resource_table.add(UserWorkerRequestBodyResource {
        body: AsyncRefCell::new(Some(tx)),
        cancel: CancelHandle::default(),
      }));
  }

  // set the request headers
  for (key, value) in req.headers {
    if !key.is_empty() {
      let header_name = HeaderName::try_from(key).unwrap();
      let mut header_value =
        HeaderValue::try_from(value).unwrap_or(HeaderValue::from_static(""));

      // if request has no body explicitly set the content-length to 0
      if !req.has_body
        && header_name == CONTENT_LENGTH
        && matches!(method, Method::POST | Method::PUT)
      {
        header_value = HeaderValue::from(0);
      }

      builder = builder.header(header_name, header_value);
    }
  }

  let req = builder
    .body(body)
    .map_err(|e| JsErrorBox::generic(format!("{:#}", e)))?;
  let request_rid = state.resource_table.add(UserWorkerRequestResource(req));

  Ok(UserWorkerBuiltRequest {
    request_rid,
    request_body_rid,
  })
}

#[op2]
#[serde]
pub async fn op_user_worker_fetch_send(
  state: Rc<RefCell<OpState>>,
  #[string] key: String,
  #[smi] rid: ResourceId,
  #[smi] request_body_rid: Option<ResourceId>,
  #[smi] stream_rid: ResourceId,
  #[smi] watcher_rid: Option<ResourceId>,
) -> Result<UserWorkerResponse, JsErrorBox> {
  let (tx, req) = {
    let (tx, mut req) = {
      let mut op_state = state.borrow_mut();
      let tx = op_state
        .borrow::<mpsc::UnboundedSender<UserWorkerMsgs>>()
        .clone();

      let req = Rc::try_unwrap(
        op_state
          .resource_table
          .take::<UserWorkerRequestResource>(rid)
          .map_err(|e| JsErrorBox::generic(format!("{:#}", e)))?,
      )
      .ok()
      .expect("multiple op_user_worker_fetch_send ongoing");

      (tx, req)
    };

    if get_upgrade_type(req.0.headers()).is_some() {
      let req_stream = state
        .borrow_mut()
        .resource_table
        .get::<HttpStreamReadResource>(stream_rid)
        .map_err(|e| JsErrorBox::generic(format!("{:#}", e)))?;

      let mut req_reader_mut =
        RcRef::map(&req_stream, |r| &r.rd).borrow_mut().await;

      if let HttpRequestReader::Headers(orig_req) = &mut *req_reader_mut
        && let Some(upgrade) = orig_req.extensions_mut().remove::<OnUpgrade>()
      {
        let _ = req.0.extensions_mut().insert(upgrade);
      }
    }

    (tx, req)
  };

  let (result_tx, result_rx) =
    oneshot::channel::<Result<SendRequestResult, Error>>();
  let key_parsed = Uuid::try_parse(key.as_str())
    .map_err(|e| JsErrorBox::generic(format!("{:#}", e)))?;

  let conn_token = watcher_rid
    .and_then(|it| {
      state
        .borrow_mut()
        .resource_table
        .take::<ConnWatcher>(it)
        .ok()
    })
    .map(Rc::try_unwrap);

  let conn_token = match conn_token {
    Some(Ok(it)) => it.get(),
    Some(Err(_)) => {
      error!("failed to unwrap connection watcher");
      None
    }

    None => None,
  };

  tx.send(UserWorkerMsgs::SendRequest(
    key_parsed,
    req.0,
    result_tx,
    conn_token.clone(),
  ))
  .map_err(|e| JsErrorBox::generic(format!("{:#}", e)))?;

  let request_body_guard = scopeguard::guard(request_body_rid, |rid| {
    if let Some(rid) = rid {
      match state
        .borrow()
        .resource_table
        .get::<UserWorkerRequestBodyResource>(rid)
      {
        Err(_) => {}
        Ok(res) => {
          res.cancel.cancel();
        }
      }
    }
  });

  let res = result_rx
    .await
    .map_err(|e| JsErrorBox::generic(format!("{:#}", e)))?;
  let (res, req_end_tx) = match res {
    Ok((res, req_end_tx)) => (res, req_end_tx),
    Err(err) => {
      error!("user worker failed to respond: {}", err);

      match err.downcast_ref() {
        Some(err @ WorkerError::RequestCancelledBySupervisor) => {
          // Use "WorkerRequestCancelled" error class to match the registered JS error class
          return Err(JsErrorBox::new(
            "WorkerRequestCancelled",
            err.to_string(),
          ));
        }
        Some(err @ WorkerError::WorkerAlreadyRetired) => {
          return Err(JsErrorBox::generic(err.to_string()));
        }

        None => {
          return Err(JsErrorBox::generic(err.to_string()));
        }
      }
    }
  };

  drop(request_body_guard);

  let mut headers = vec![];
  for (key, value) in res.headers().iter() {
    headers.push((
      ByteString::from(key.as_str()),
      ByteString::from(value.to_str().unwrap_or_default()),
    ));
  }

  let status = res.status().as_u16();
  let status_text = res
    .status()
    .canonical_reason()
    .unwrap_or("<unknown status code>")
    .to_string();

  let size = HttpBody::size_hint(res.body()).exact();
  let stream: BytesStream =
    Box::pin(res.into_body().map(|r| r.map_err(std::io::Error::other)));

  let mut op_state = state.borrow_mut();

  let body_rid = op_state.resource_table.add(UserWorkerResponseBodyResource {
    reader: AsyncRefCell::new(stream.peekable()),
    cancel: CancelHandle::default(),
    size,
    req_end_tx,
    conn_token,
  });

  let response = UserWorkerResponse {
    status,
    status_text,
    headers,
    body_rid,
    size,
  };

  Ok(response)
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

/// Wraps a [`mpsc::Receiver`] in a [`Stream`] that can be used as a Hyper [`Body`].
pub struct BodyStream(pub mpsc::Receiver<Result<bytes::Bytes, Error>>);

impl Stream for BodyStream {
  type Item = Result<bytes::Bytes, Error>;

  fn poll_next(
    mut self: Pin<&mut Self>,
    cx: &mut Context<'_>,
  ) -> Poll<Option<Self::Item>> {
    self.0.poll_recv(cx)
  }
}
