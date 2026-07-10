//! Integration tests for the flow user-worker runtime.
//!
//! Workers are booted directly (no HTTP: flow's comm surface is MessagePort,
//! which is exercised end-to-end by the spec tests driving the real binary).
//! These tests observe workers through their lifecycle events instead: the
//! console interceptor forwards `console.*` output as `Log` events and the
//! supervisor reports the shutdown reason, both over the events channel that
//! `edge/cli` normally wires to `FlowRuntime.events`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use base::flags::WorkerFlags;
use base::utils::test_utils::create_test_user_worker;
use base::utils::test_utils::test_user_runtime_opts;
use base::worker::TerminationToken;
use base::worker::WorkerSurface;
use base::worker::WorkerSurfaceBuilder;
use base::worker::create_user_worker_pool;
use base::worker::pool::SupervisorPolicy;
use base::worker::pool::WorkerPoolPolicy;
use deno_facade::EszipPayloadKind;
use either::Either;
use ext_event_worker::events::LogLevel;
use ext_event_worker::events::ShutdownReason;
use ext_event_worker::events::WorkerEventWithMetadata;
use ext_event_worker::events::WorkerEvents;
use ext_workers::context::UserWorkerMsgs;
use ext_workers::context::UserWorkerRuntimeOpts;
use ext_workers::context::WorkerContextInitOpts;
use serial_test::serial;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;
use uuid::Uuid;

const EVENT_DEADLINE: Duration = Duration::from_secs(60);

fn fixture(path: &str) -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)
}

fn base_init_opts(path: &str) -> WorkerContextInitOpts {
  WorkerContextInitOpts {
    service_path: fixture(path),
    no_module_cache: false,
    no_npm: None,
    env_vars: HashMap::new(),
    timing: None,
    maybe_eszip: None,
    maybe_entrypoint: None,
    maybe_module_code: None,
    conf: Box::new(test_user_runtime_opts()),
    static_patterns: vec![],
    maybe_s3_fs_config: None,
    maybe_tmp_fs_config: None,
    maybe_http_fs_config: None,
    maybe_otel_config: None,
  }
}

struct TestWorker {
  #[allow(dead_code, reason = "keeps the worker surface alive")]
  surface: WorkerSurface,
  termination_token: TerminationToken,
  events_rx: mpsc::UnboundedReceiver<WorkerEventWithMetadata>,
}

impl TestWorker {
  /// Boot a user worker for `path`, with `tweak` applied to the runtime opts
  /// and `flags` fed to the supervisor. Panics on boot failure.
  async fn boot(
    path: &str,
    policy: SupervisorPolicy,
    flags: WorkerFlags,
    tweak: impl FnOnce(&mut UserWorkerRuntimeOpts),
  ) -> Self {
    Self::try_boot(path, policy, flags, tweak).await.unwrap()
  }

  async fn try_boot(
    path: &str,
    policy: SupervisorPolicy,
    flags: WorkerFlags,
    tweak: impl FnOnce(&mut UserWorkerRuntimeOpts),
  ) -> Result<Self, anyhow::Error> {
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let termination_token = TerminationToken::new();

    let mut opts = base_init_opts(path);
    opts.conf.key = Some(Uuid::new_v4());
    opts.conf.service_path =
      Some(opts.service_path.to_string_lossy().into_owned());
    opts.conf.events_msg_tx = Some(events_tx);
    tweak(&mut opts.conf);

    let surface = WorkerSurfaceBuilder::new()
      .init_opts(opts)
      .policy(policy)
      .worker_flags(Either::Right(flags))
      .termination_token(termination_token.clone())
      .eager_module_init(true)
      .build()
      .await?;

    Ok(Self {
      surface,
      termination_token,
      events_rx,
    })
  }

  /// Wait until an event matching `pred` arrives (within `EVENT_DEADLINE`).
  /// Panics when the deadline passes or the channel closes first.
  async fn expect_event(
    &mut self,
    what: &str,
    mut pred: impl FnMut(&WorkerEvents) -> bool,
  ) -> WorkerEventWithMetadata {
    let fut = async {
      while let Some(ev) = self.events_rx.recv().await {
        if pred(&ev.event) {
          return ev;
        }
      }
      panic!("events channel closed before observing: {what}");
    };

    match timeout(EVENT_DEADLINE, fut).await {
      Ok(ev) => ev,
      Err(_) => panic!("timed out waiting for: {what}"),
    }
  }

  async fn expect_log_containing(&mut self, needle: &str) {
    let needle_owned = needle.to_string();
    self
      .expect_event(&format!("log containing {needle:?}"), move |ev| {
        matches!(
          ev,
          WorkerEvents::Log(log)
            if log.level == LogLevel::Info && log.msg.contains(&needle_owned)
        )
      })
      .await;
  }

  async fn expect_shutdown_reason(&mut self, reason: ShutdownReason) {
    self
      .expect_event(
        &format!("shutdown with reason {reason:?}"),
        |ev| matches!(ev, WorkerEvents::Shutdown(s) if s.reason == reason),
      )
      .await;
  }

  /// Request graceful termination and wait for the worker to wind down.
  async fn terminate(&self) {
    let fut = self.termination_token.cancel_and_wait();
    if timeout(Duration::from_secs(30), fut).await.is_err() {
      panic!("worker did not terminate in time");
    }
  }
}

// ---------------------------------------------------------------------------
// Boot & module evaluation
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_worker_boot_and_json_imports() {
  let mut worker = TestWorker::boot(
    "test_cases/json_import",
    SupervisorPolicy::PerWorker,
    WorkerFlags::default(),
    |_| {},
  )
  .await;

  worker
    .expect_log_containing("json_import test passed")
    .await;
  worker.terminate().await;
}

#[tokio::test]
#[serial]
async fn test_worker_imports_npm() {
  let mut worker = TestWorker::boot(
    "test_cases/npm",
    SupervisorPolicy::PerWorker,
    WorkerFlags::default(),
    |_| {},
  )
  .await;

  worker.expect_log_containing("npm test passed").await;
  worker.terminate().await;
}

#[tokio::test]
#[serial]
async fn test_worker_boot_invalid_imports() {
  let result =
    create_test_user_worker(base_init_opts("test_cases/invalid_imports")).await;

  assert!(result.is_err());
  assert!(
    result
      .unwrap_err()
      .to_string()
      .starts_with("worker boot error")
  );
}

#[tokio::test]
#[serial]
async fn test_worker_boot_with_0_byte_eszip() {
  let mut opts = base_init_opts("test_cases/meow");
  opts.maybe_eszip = Some(EszipPayloadKind::VecKind(vec![]));
  opts.maybe_entrypoint = Some("file:///src/index.ts".to_string());

  let result = create_test_user_worker(opts).await;

  assert!(result.is_err());
  assert!(format!("{:#}", result.unwrap_err()).starts_with(
    "worker boot error: failed to bootstrap runtime: unexpected end of file"
  ));
}

#[tokio::test]
#[serial]
async fn test_worker_boot_with_invalid_entrypoint() {
  let mut opts = base_init_opts("test_cases/meow");
  opts.maybe_entrypoint = Some("file:///meow/mmmmeeeow.ts".to_string());

  let result = create_test_user_worker(opts).await;

  assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Sandbox surface
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_serve_and_listen_are_denied() {
  let mut worker = TestWorker::boot(
    "test_cases/serve-denied",
    SupervisorPolicy::PerWorker,
    WorkerFlags::default(),
    |_| {},
  )
  .await;

  worker
    .expect_log_containing("serve-denied test passed")
    .await;
  worker.terminate().await;
}

#[tokio::test]
#[serial]
async fn test_host_fs_access_blocked_by_default() {
  let mut worker = TestWorker::boot(
    "test_cases/host-fs-access",
    SupervisorPolicy::PerWorker,
    WorkerFlags::default(),
    |_| {},
  )
  .await;

  worker.expect_log_containing("host-fs-access denied").await;
  worker.terminate().await;
}

#[tokio::test]
#[serial]
async fn test_host_fs_access_allowed() {
  let mut worker = TestWorker::boot(
    "test_cases/host-fs-access",
    SupervisorPolicy::PerWorker,
    WorkerFlags::default(),
    |conf| {
      conf.allow_host_fs_access = Some(true);
    },
  )
  .await;

  worker.expect_log_containing("host-fs-access ok").await;
  worker.terminate().await;
}

#[tokio::test]
#[serial]
async fn test_restrict_host_fs_rejects_allow_host_fs_access() {
  let result = TestWorker::try_boot(
    "test_cases/host-fs-access",
    SupervisorPolicy::PerWorker,
    WorkerFlags {
      restrict_host_fs: true,
      ..Default::default()
    },
    |conf| {
      conf.allow_host_fs_access = Some(true);
    },
  )
  .await;

  let err = format!("{:#}", result.err().unwrap());
  assert!(
    err.contains("allowHostFsAccess cannot be enabled"),
    "unexpected error: {err}"
  );
}

// ---------------------------------------------------------------------------
// Resource limits (supervisor kill reasons)
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_wall_clock_limit_kills_worker() {
  let mut worker = TestWorker::boot(
    "test_cases/sleep-top-level",
    SupervisorPolicy::PerWorker,
    WorkerFlags::default(),
    |conf| {
      conf.worker_timeout_ms = 1_000;
    },
  )
  .await;

  worker
    .expect_shutdown_reason(ShutdownReason::WallClockTime)
    .await;
}

#[tokio::test]
#[serial]
async fn test_cpu_time_limit_kills_worker() {
  let mut worker = TestWorker::boot(
    "test_cases/cpu-sync",
    SupervisorPolicy::PerWorker,
    WorkerFlags::default(),
    |conf| {
      conf.cpu_time_soft_limit_ms = 250;
      conf.cpu_time_hard_limit_ms = 500;
    },
  )
  .await;

  worker.expect_shutdown_reason(ShutdownReason::CPUTime).await;
}

#[tokio::test]
#[serial]
async fn test_memory_limit_kills_worker() {
  let mut worker = TestWorker::boot(
    "test_cases/heap_limit",
    SupervisorPolicy::PerWorker,
    WorkerFlags::default(),
    |conf| {
      conf.memory_limit_mb = 30;
    },
  )
  .await;

  worker.expect_shutdown_reason(ShutdownReason::Memory).await;
}

// ---------------------------------------------------------------------------
// beforeunload dispatch (threshold ratios)
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_runtime_event_beforeunload_cpu() {
  let mut worker = TestWorker::boot(
    "test_cases/runtime-event-cpu",
    SupervisorPolicy::PerWorker,
    WorkerFlags {
      beforeunload_cpu_pct: Some(50),
      ..Default::default()
    },
    |conf| {
      conf.cpu_time_soft_limit_ms = 2_000;
      conf.cpu_time_hard_limit_ms = 4_000;
    },
  )
  .await;

  worker.expect_log_containing("triggered cpu").await;
  worker.terminate().await;
}

#[tokio::test]
#[serial]
async fn test_runtime_event_beforeunload_wall_clock() {
  let mut worker = TestWorker::boot(
    "test_cases/runtime-event-wall-clock",
    SupervisorPolicy::PerWorker,
    WorkerFlags {
      beforeunload_wall_clock_pct: Some(50),
      ..Default::default()
    },
    |conf| {
      conf.worker_timeout_ms = 4_000;
    },
  )
  .await;

  worker.expect_log_containing("triggered wall_clock").await;
}

#[tokio::test]
#[serial]
async fn test_runtime_event_beforeunload_mem() {
  let mut worker = TestWorker::boot(
    "test_cases/runtime-event-mem",
    SupervisorPolicy::PerWorker,
    WorkerFlags {
      beforeunload_memory_pct: Some(50),
      ..Default::default()
    },
    |conf| {
      conf.memory_limit_mb = 50;
    },
  )
  .await;

  worker.expect_log_containing("triggered memory").await;
}

// ---------------------------------------------------------------------------
// Background tasks
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_wait_until_keeps_worker_alive() {
  let mut worker = TestWorker::boot(
    "test_cases/background-task",
    SupervisorPolicy::PerWorker,
    WorkerFlags::default(),
    |_| {},
  )
  .await;

  worker
    .expect_log_containing("background-task main finished")
    .await;
  worker.expect_log_containing("background task done").await;
  worker.terminate().await;
}

// ---------------------------------------------------------------------------
// Worker pool
// ---------------------------------------------------------------------------

async fn pool_create_worker(
  pool_tx: &mpsc::UnboundedSender<UserWorkerMsgs>,
  path: &str,
) -> ext_workers::context::CreateUserWorkerResult {
  let (result_tx, result_rx) = oneshot::channel();
  let mut opts = base_init_opts(path);
  opts.conf.force_create = false;

  pool_tx
    .send(UserWorkerMsgs::Create(Box::new(opts), result_tx))
    .unwrap();

  timeout(EVENT_DEADLINE, result_rx)
    .await
    .expect("timed out waiting for worker creation")
    .unwrap()
    .expect("worker creation failed")
}

#[tokio::test]
#[serial]
async fn test_pool_reuses_active_worker() {
  let termination_token = TerminationToken::new();
  let (_metrics, pool_tx) = create_user_worker_pool(
    Default::default(),
    WorkerPoolPolicy::new(
      SupervisorPolicy::PerWorker,
      1,
      WorkerFlags {
        request_wait_timeout_ms: Some(10_000),
        ..Default::default()
      },
    ),
    None,
    Some(termination_token.clone()),
    vec![],
    None,
  )
  .await
  .unwrap();

  let first = pool_create_worker(&pool_tx, "test_cases/background-task").await;
  assert!(!first.reused);

  let second = pool_create_worker(&pool_tx, "test_cases/background-task").await;
  assert!(second.reused);
  assert_eq!(first.key, second.key);

  termination_token.cancel_and_wait().await;
}

#[tokio::test]
#[serial]
async fn test_pool_cleanup_idle_workers() {
  let termination_token = TerminationToken::new();
  let (_metrics, pool_tx) = create_user_worker_pool(
    Default::default(),
    WorkerPoolPolicy::new(
      SupervisorPolicy::PerWorker,
      1,
      WorkerFlags {
        request_wait_timeout_ms: Some(10_000),
        ..Default::default()
      },
    ),
    None,
    Some(termination_token.clone()),
    vec![],
    None,
  )
  .await
  .unwrap();

  let created = pool_create_worker(&pool_tx, "test_cases/json_import").await;
  assert!(!created.reused);

  // The fixture completes immediately, so the worker should agree to an
  // early drop once its module evaluation is done.
  let mut dropped = 0;
  for _ in 0..50 {
    let (tx, rx) = oneshot::channel();
    pool_tx
      .send(UserWorkerMsgs::TryCleanupIdleWorkers(1_000, tx))
      .unwrap();
    dropped = timeout(EVENT_DEADLINE, rx).await.unwrap().unwrap();
    if dropped > 0 {
      break;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
  }
  assert_eq!(dropped, 1);

  termination_token.cancel_and_wait().await;
}
