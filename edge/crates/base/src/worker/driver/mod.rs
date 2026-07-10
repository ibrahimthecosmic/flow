use std::future::Future;

use anyhow::Context;
use anyhow::Error;
use ext_event_worker::events::WorkerEvents;
use tokio::sync::oneshot;
use tokio::task::JoinError;

use crate::runtime::DenoRuntime;

mod user;

struct BaseCx {
  termination_event_tx: Option<oneshot::Sender<WorkerEvents>>,
  termination_event_rx: Option<oneshot::Receiver<WorkerEvents>>,
}

impl Default for BaseCx {
  fn default() -> Self {
    let (termination_event_tx, termination_event_rx) = oneshot::channel();

    Self {
      termination_event_tx: Some(termination_event_tx),
      termination_event_rx: Some(termination_event_rx),
    }
  }
}

impl BaseCx {
  fn take_termination_event_sender(
    &mut self,
  ) -> Result<oneshot::Sender<WorkerEvents>, Error> {
    self
      .termination_event_tx
      .take()
      .context("termination_event_tx already been consumed")
  }

  fn take_termination_event_receiver(
    &mut self,
  ) -> Result<oneshot::Receiver<WorkerEvents>, Error> {
    self
      .termination_event_rx
      .take()
      .context("termination_event_rx already been consumed")
  }
}

/// The one worker driver: flow only boots (supervised) user workers.
/// (`User::new(init_opts, cx)` is the constructor.)
pub(crate) type WorkerDriverImpl = user::User;

pub(super) trait WorkerDriver: Send {
  fn on_created<'l>(
    &self,
    runtime: &'l mut DenoRuntime,
  ) -> impl Future<Output = Result<WorkerEvents, Error>> + 'l;

  fn supervise(
    &self,
    runtime: &mut DenoRuntime,
  ) -> Option<impl Future<Output = Result<(), JoinError>> + 'static>;
}
