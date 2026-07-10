use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Context;
use either::Either;
use ext_event_worker::events::BootEvent;
use ext_event_worker::events::WorkerEvents;
use ext_workers::context::WorkerContextInitOpts;
use ext_workers::context::WorkerExit;
use tokio::sync::oneshot;

use super::WorkerBuilder;
use super::WorkerSurface;
use super::pool::SupervisorPolicy;
use super::termination_token::TerminationToken;
use super::utils::send_event_if_event_worker_available;
use crate::flags::WorkerFlags;
use crate::inspector_server::Inspector;

pub type WorkerBuilderHook =
  Box<dyn FnOnce(&mut WorkerBuilder) -> Result<(), anyhow::Error> + Send>;

pub struct WorkerSurfaceBuilder {
  init_opts: Option<WorkerContextInitOpts>,
  flags: Option<Arc<WorkerFlags>>,
  policy: Option<SupervisorPolicy>,
  termination_token: Option<TerminationToken>,
  inspector: Option<Inspector>,
  worker_builder_hook: Option<WorkerBuilderHook>,
  eager_module_init: bool,
}

impl Default for WorkerSurfaceBuilder {
  fn default() -> Self {
    Self::new()
  }
}

impl WorkerSurfaceBuilder {
  pub fn new() -> Self {
    Self {
      init_opts: None,
      flags: None,
      policy: None,
      termination_token: None,
      inspector: None,
      worker_builder_hook: None,
      eager_module_init: false,
    }
  }

  pub fn init_opts(mut self, value: WorkerContextInitOpts) -> Self {
    self.init_opts = Some(value);
    self
  }

  pub fn worker_flags(
    mut self,
    value: Either<Arc<WorkerFlags>, WorkerFlags>,
  ) -> Self {
    self.flags = Some(value.map_right(Arc::new).into_inner());
    self
  }

  pub fn policy(mut self, value: SupervisorPolicy) -> Self {
    self.policy = Some(value);
    self
  }

  pub fn termination_token(mut self, value: TerminationToken) -> Self {
    self.termination_token = Some(value);
    self
  }

  pub fn inspector(mut self, value: Inspector) -> Self {
    self.inspector = Some(value);
    self
  }

  pub fn worker_builder_hook<F>(mut self, value: F) -> Self
  where
    F: FnOnce(&mut WorkerBuilder) -> Result<(), anyhow::Error> + Send + 'static,
  {
    self.worker_builder_hook = Some(Box::new(value) as _);
    self
  }

  pub fn eager_module_init(mut self, value: bool) -> Self {
    self.eager_module_init = value;
    self
  }

  pub fn set_init_opts(
    &mut self,
    value: Option<WorkerContextInitOpts>,
  ) -> &mut Self {
    self.init_opts = value;
    self
  }

  pub fn set_worker_flags(
    &mut self,
    value: Option<Either<Arc<WorkerFlags>, WorkerFlags>>,
  ) -> &mut Self {
    self.flags = value.map(|it| it.map_right(Arc::new).into_inner());
    self
  }

  pub fn set_policy(&mut self, value: Option<SupervisorPolicy>) -> &mut Self {
    self.policy = value;
    self
  }

  pub fn set_termination_token(
    &mut self,
    value: Option<TerminationToken>,
  ) -> &mut Self {
    self.termination_token = value;
    self
  }

  pub fn set_inspector(&mut self, value: Option<Inspector>) -> &mut Self {
    self.inspector = value;
    self
  }

  pub fn set_worker_builder_hook<F>(&mut self, value: Option<F>) -> &mut Self
  where
    F: FnOnce(&mut WorkerBuilder) -> Result<(), anyhow::Error> + Send + 'static,
  {
    self.worker_builder_hook = value.map(|it| Box::new(it) as _);
    self
  }

  pub fn set_eager_module_init(&mut self, value: bool) -> &mut Self {
    self.eager_module_init = value;
    self
  }

  pub async fn build(self) -> Result<WorkerSurface, anyhow::Error> {
    let Self {
      init_opts,
      flags,
      policy,
      termination_token,
      inspector,
      worker_builder_hook,
      eager_module_init,
    } = self;

    let (worker_boot_result_tx, worker_boot_result_rx) = oneshot::channel::<
      Result<tokio_util::sync::CancellationToken, anyhow::Error>,
    >();

    let flags = flags.unwrap_or_default();
    let init_opts = init_opts.context("init_opts must be specified")?;
    let exit = WorkerExit::default();
    let mut worker_builder = WorkerBuilder::new(init_opts, flags);

    worker_builder
      .set_inspector(inspector)
      .set_supervisor_policy(policy)
      .set_termination_token(termination_token.clone());

    if let Some(hook) = worker_builder_hook {
      hook(&mut worker_builder)?;
    }

    let worker = worker_builder.build()?;
    let cx = worker.cx.clone();

    let thread_handles = worker
      .start(eager_module_init, worker_boot_result_tx, exit.clone())
      .await;

    // wait for worker to be successfully booted
    match worker_boot_result_rx.await? {
      Ok(cancel) => {
        let elapsed = cx.worker_boot_start_time.elapsed().as_millis();

        send_event_if_event_worker_available(
          cx.events_msg_tx.as_ref(),
          WorkerEvents::Boot(BootEvent {
            boot_time: elapsed as usize,
          }),
          cx.event_metadata.clone(),
        );

        Ok(WorkerSurface {
          exit,
          cancel,
          thread_handles: Arc::new(Mutex::new(thread_handles)),
        })
      }

      Err(err) => {
        if let Some(token) = termination_token.as_ref() {
          token.outbound.cancel();
        }

        Err(err)
      }
    }
  }
}
