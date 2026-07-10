#![allow(
  dead_code,
  reason = "test helpers; each test target uses only a subset"
)]

use std::marker::PhantomPinned;
use std::path::Path;
use std::sync::Arc;
use std::task::Poll;
use std::task::ready;

use anyhow::Error;
use ext_workers::context::Timing;
use ext_workers::context::UserWorkerRuntimeOpts;
use ext_workers::context::WorkerContextInitOpts;
use futures_util::Future;
use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use pin_project::pin_project;
use tokio::process::Command;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::flags::WorkerFlags;
use crate::worker;
use crate::worker::TerminationToken;
use crate::worker::pool::SupervisorPolicy;
use crate::worker::pool::WorkerPoolPolicy;

pub struct CreateTestUserWorkerArgs(
  WorkerContextInitOpts,
  Option<SupervisorPolicy>,
);

impl From<WorkerContextInitOpts> for CreateTestUserWorkerArgs {
  fn from(val: WorkerContextInitOpts) -> Self {
    Self(val, None)
  }
}

impl From<(WorkerContextInitOpts, SupervisorPolicy)>
  for CreateTestUserWorkerArgs
{
  fn from(val: (WorkerContextInitOpts, SupervisorPolicy)) -> Self {
    Self(val.0, Some(val.1))
  }
}

/// Drives a worker's `Timing::req` scope channels, so tests can simulate
/// units of active work for the request-scoped supervisor policies
/// (`per_request` / `oneshot`).
#[derive(Debug)]
pub struct RequestScope {
  policy: SupervisorPolicy,
  req_start_tx: mpsc::UnboundedSender<Arc<Notify>>,
  req_end_tx: mpsc::UnboundedSender<()>,
  termination_token: TerminationToken,
  conn_token: CancellationToken,
}

impl RequestScope {
  pub fn conn_token(&self) -> CancellationToken {
    self.conn_token.clone()
  }

  pub async fn start_request(self) -> RequestScopeGuard {
    if self.policy.is_per_request() {
      let fence = Arc::<Notify>::default();

      self.req_start_tx.send(fence.clone()).unwrap();
      fence.notified().await;
    }

    RequestScopeGuard {
      cancelled: false,
      req_end_tx: self.req_end_tx.clone(),
      termination_token: Some(self.termination_token.clone()),
      conn_token: self.conn_token.clone(),
      inner: None,
      _pinned: PhantomPinned,
    }
  }
}

#[pin_project]
pub struct RequestScopeGuard {
  cancelled: bool,
  req_end_tx: mpsc::UnboundedSender<()>,
  termination_token: Option<TerminationToken>,
  conn_token: CancellationToken,
  inner: Option<BoxFuture<'static, ()>>,
  _pinned: PhantomPinned,
}

impl Future for RequestScopeGuard {
  type Output = ();

  fn poll(
    self: std::pin::Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> std::task::Poll<Self::Output> {
    let this = self.project();

    if !(*this.cancelled) {
      *this.cancelled = true;
      this.req_end_tx.send(()).unwrap();
      this.termination_token.as_ref().unwrap().inbound.cancel();
    }

    let inner = this.inner.get_or_insert_with(|| {
      wait_termination(this.termination_token.take().unwrap()).boxed()
    });

    ready!(inner.as_mut().poll_unpin(cx));
    this.conn_token.cancel();

    Poll::Ready(())
  }
}

pub async fn create_test_user_worker<Opt: Into<CreateTestUserWorkerArgs>>(
  opts: Opt,
) -> Result<(worker::WorkerSurface, RequestScope), Error> {
  let CreateTestUserWorkerArgs(mut opts, maybe_policy) = opts.into();
  let (req_start_tx, req_start_rx) = mpsc::unbounded_channel();
  let (req_end_tx, req_end_rx) = mpsc::unbounded_channel();

  let policy = maybe_policy.unwrap_or_else(SupervisorPolicy::oneshot);
  let termination_token = TerminationToken::new();

  opts.timing = Some(Timing {
    req: (req_start_rx, req_end_rx),
    ..Default::default()
  });

  let worker_surface = worker::WorkerSurfaceBuilder::new()
    .init_opts(opts)
    .policy(policy)
    .termination_token(termination_token.clone())
    .build()
    .await?;

  Ok((
    worker_surface,
    RequestScope {
      policy,
      req_start_tx,
      req_end_tx,
      termination_token,
      conn_token: CancellationToken::new(),
    },
  ))
}

pub fn test_user_worker_pool_policy() -> WorkerPoolPolicy {
  WorkerPoolPolicy::new(
    SupervisorPolicy::oneshot(),
    1,
    WorkerFlags {
      request_wait_timeout_ms: Some(4 * 1000 * 3600),
      ..Default::default()
    },
  )
}

pub fn test_user_runtime_opts() -> UserWorkerRuntimeOpts {
  UserWorkerRuntimeOpts {
    worker_timeout_ms: 4 * 1000 * 3600,
    cpu_time_soft_limit_ms: 4 * 1000 * 3600,
    cpu_time_hard_limit_ms: 4 * 1000 * 3600,
    ..Default::default()
  }
}

pub async fn ensure_npm_package_installed<P>(path: P)
where
  P: AsRef<Path>,
{
  let cwd = std::env::current_dir().unwrap();
  let path = cwd.join(path);

  assert!(path.is_dir());

  let output = Command::new("npm")
    .current_dir(path)
    .arg("i")
    .output()
    .await
    .unwrap();

  if !output.status.success() {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    panic!(
      "failed to execute npm command\n\nSTDOUT: ${stdout}\n\nSTDERR: ${stderr}"
    );
  }
}

async fn wait_termination(token: TerminationToken) {
  token.outbound.cancelled().await;
}
