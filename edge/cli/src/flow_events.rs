//! flow: the `FlowRuntime.events` backend - a single-consumer stream of
//! user-worker lifecycle/log events (edge's `WorkerEventWithMetadata`) fed by
//! the pool's always-on event channel.
//!
//! Ownership model ("stdio inherit until claimed"):
//!
//!   - At startup a relay task owns the receiver and DRAINS it, printing with
//!     Node `stdio: "inherit"`-like semantics: user-worker `console` output
//!     goes to flow's stdout/stderr by level, crashes go to stderr, and
//!     lifecycle telemetry (`Boot`/`Shutdown`) stays quiet (debug log only) -
//!     a child process does not print "I booted".
//!   - When JS starts iterating `FlowRuntime.events`, `op_flow_events_claim`
//!     asks the relay to hand the receiver over into the main isolate's
//!     op_state; `op_flow_events_accept` then serves the iterator with the
//!     same take/await/put-back pattern as edge's events-worker op. A second
//!     concurrent claim fails: the stream is single-consumer by design.
//!   - When the consumer stops (loop break / iterator `return()`),
//!     `op_flow_events_release` hands the receiver back and inherit-mode
//!     draining resumes. No event is ever dropped in the handover: `recv()`
//!     is cancel-safe, so the `select!` below cannot lose one.
//!
//! This replaces edge's dedicated events-worker isolate (`--event-worker`,
//! `WorkerKind::EventsWorker`): flow's trusted main isolate is the consumer,
//! so the whole pattern collapses into a host API. The yielded JS shape is
//! kept identical to edge/trex's `EventManager` (see flow_main.js).

use std::cell::RefCell;
use std::rc::Rc;

use deno_core::CancelFuture;
use deno_core::CancelHandle;
use deno_core::OpState;
use deno_core::op2;
use deno_error::JsErrorBox;
use ext_event_worker::events::LogLevel;
use ext_event_worker::events::RawEvent;
use ext_event_worker::events::WorkerEventWithMetadata;
use ext_event_worker::events::WorkerEvents;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// Commands from the main-isolate ops to the relay task.
pub enum RelayCmd {
  /// Hand the event receiver to the claimer (`FlowRuntime.events` starting to
  /// iterate). The reply is `None` when another consumer already holds it.
  Claim(
    oneshot::Sender<Option<mpsc::UnboundedReceiver<WorkerEventWithMetadata>>>,
  ),
  /// Return the receiver after the consumer stopped iterating; inherit-mode
  /// draining resumes.
  Release(mpsc::UnboundedReceiver<WorkerEventWithMetadata>),
}

/// Spawns the relay/drain task on the current tokio runtime and returns the
/// command sender the ops use to claim/release the stream. Must be called
/// where `tokio::spawn` is available (flow's post-bootstrap hook is).
pub fn spawn_events_relay(
  rx: mpsc::UnboundedReceiver<WorkerEventWithMetadata>,
) -> mpsc::UnboundedSender<RelayCmd> {
  let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<RelayCmd>();

  tokio::spawn(async move {
    // `Some` = unclaimed (this task drains + prints), `None` = claimed by JS.
    let mut slot = Some(rx);

    loop {
      match slot.take() {
        Some(mut rx) => loop {
          tokio::select! {
            biased;

            cmd = cmd_rx.recv() => match cmd {
              Some(RelayCmd::Claim(reply)) => {
                match reply.send(Some(rx)) {
                  // Claimed; wait for release in the outer loop.
                  Ok(()) => break,
                  // The claiming op future was dropped before it could take
                  // the receiver (isolate teardown mid-claim): keep draining.
                  Err(unsent) => {
                    rx = unsent.expect("reply payload was Some");
                  }
                }
              }
              // A release while this task already holds the receiver cannot
              // happen (single claimer); ignore defensively.
              Some(RelayCmd::Release(_)) => {}
              // op_state dropped (isolate teardown): no claimer can ever
              // appear again, but keep printing late worker output until the
              // pool closes the channel.
              None => {
                while let Some(ev) = rx.recv().await {
                  print_inherit(&ev);
                }
                return;
              }
            },

            ev = rx.recv() => match ev {
              Some(ev) => print_inherit(&ev),
              // Pool shut down; nothing left to relay.
              None => return,
            },
          }
        },

        // Claimed: hold no receiver, only answer commands.
        None => match cmd_rx.recv().await {
          Some(RelayCmd::Claim(reply)) => {
            let _ = reply.send(None); // already claimed
          }
          Some(RelayCmd::Release(rx)) => slot = Some(rx),
          None => return,
        },
      }
    }
  });

  cmd_tx
}

/// Prints one event the way a Node child with `stdio: "inherit"` would appear:
/// its console output lands on our stdout/stderr, its crash on stderr, and its
/// process lifecycle makes no noise.
fn print_inherit(ev: &WorkerEventWithMetadata) {
  match &ev.event {
    // Worker console messages arrive with their trailing newline already
    // attached (edge's console interceptor) - write them verbatim, only
    // supplying a newline when one is missing.
    WorkerEvents::Log(log) => match log.level {
      LogLevel::Debug | LogLevel::Info => {
        write_inherit(std::io::stdout().lock(), &log.msg)
      }
      LogLevel::Warning | LogLevel::Error => {
        write_inherit(std::io::stderr().lock(), &log.msg)
      }
    },
    WorkerEvents::BootFailure(e) => {
      let msg = format!("flow: worker boot failure: {}", e.msg);
      write_inherit(std::io::stderr().lock(), &msg)
    }
    WorkerEvents::UncaughtException(e) => {
      write_inherit(std::io::stderr().lock(), &e.exception)
    }
    other @ (WorkerEvents::Boot(_)
    | WorkerEvents::Shutdown(_)
    | WorkerEvents::BundleCache(_)) => {
      log::debug!("flow: worker event: {other:?}");
    }
  }
}

/// Maps a bundle-cache notice onto the worker-event stream shape
/// (`FlowRuntime.events` yields it as `event_type: "BundleCache"` with
/// empty metadata — it is runtime-global, not tied to a worker).
pub fn bundle_cache_event(
  event: deno_facade::bundle_cache::CacheEvent,
) -> WorkerEventWithMetadata {
  use deno_facade::bundle_cache::CacheEventAction;

  WorkerEventWithMetadata {
    event: WorkerEvents::BundleCache(
      ext_event_worker::events::BundleCacheEvent {
        action: match event.action {
          CacheEventAction::Evicted => "evicted",
          CacheEventAction::OverCap => "overCap",
          CacheEventAction::Sweep => "sweep",
        }
        .to_string(),
        cache_key: event.cache_key,
        path: event.path.map(|it| it.to_string_lossy().into_owned()),
        bytes: event.bytes,
        total_bytes: event.total_bytes,
        max_bytes: event.max_bytes,
      },
    ),
    metadata: Default::default(),
  }
}

fn write_inherit(mut out: impl std::io::Write, msg: &str) {
  let _ = out.write_all(msg.as_bytes());
  if !msg.ends_with('\n') {
    let _ = out.write_all(b"\n");
  }
  let _ = out.flush();
}

/// Cancels a pending `op_flow_events_accept` (iterator `return()` while a
/// `next()` is in flight). Op_state-typed wrapper around the claim-scoped
/// cancel handle.
struct EventsCancelHandle(Rc<CancelHandle>);

/// Claims the event stream for the calling isolate: moves the receiver from
/// the relay task into op_state, where `op_flow_events_accept` serves it.
/// Fails if another consumer currently holds the stream (single-consumer).
#[op2]
async fn op_flow_events_claim(
  state: Rc<RefCell<OpState>>,
) -> Result<(), JsErrorBox> {
  let cmd_tx = state
    .borrow()
    .try_borrow::<mpsc::UnboundedSender<RelayCmd>>()
    .cloned();
  let Some(cmd_tx) = cmd_tx else {
    return Err(JsErrorBox::generic("flow events relay is not available"));
  };

  let (reply_tx, reply_rx) = oneshot::channel();
  if cmd_tx.send(RelayCmd::Claim(reply_tx)).is_err() {
    return Err(JsErrorBox::generic("flow events relay is gone"));
  }

  match reply_rx.await {
    Ok(Some(rx)) => {
      let mut op_state = state.borrow_mut();
      op_state.put(rx);
      // Fresh handle per claim: a cancel only ever ends the claim it was
      // issued under, never a later one.
      op_state.put(EventsCancelHandle(Rc::new(CancelHandle::new())));
      Ok(())
    }
    Ok(None) => Err(JsErrorBox::generic(
      "FlowRuntime.events is already claimed by another consumer",
    )),
    Err(_) => Err(JsErrorBox::generic("flow events relay is gone")),
  }
}

/// Awaits the next event on the claimed stream. Same take/await/put-back
/// pattern as edge's `op_event_accept`: the receiver leaves op_state for the
/// duration of the await, which also makes concurrent accepts fail loudly
/// instead of silently competing.
#[op2]
#[serde]
async fn op_flow_events_accept(
  state: Rc<RefCell<OpState>>,
) -> Result<RawEvent, JsErrorBox> {
  let rx = state
    .borrow_mut()
    .try_take::<mpsc::UnboundedReceiver<WorkerEventWithMetadata>>();
  let Some(mut rx) = rx else {
    return Err(JsErrorBox::generic(
      "FlowRuntime.events is not claimed (or another accept is pending)",
    ));
  };

  let cancel = state
    .borrow()
    .try_borrow::<EventsCancelHandle>()
    .map(|h| h.0.clone());

  let data = match cancel {
    Some(handle) => match rx.recv().or_cancel(handle).await {
      Ok(data) => data,
      // Cancelled by `op_flow_events_cancel` (iterator return() with this
      // accept in flight). Surface it as Done: the JS side is ending the
      // iteration anyway and will release the receiver right after.
      Err(_) => {
        state.borrow_mut().put(rx);
        return Ok(RawEvent::Done);
      }
    },
    None => rx.recv().await,
  };

  let mut op_state = state.borrow_mut();
  op_state.put(rx);

  match data {
    Some(event) => Ok(RawEvent::Event(Box::new(event))),
    None => {
      op_state.waker.wake();
      Ok(RawEvent::Done)
    }
  }
}

/// Interrupts a pending `op_flow_events_accept`, making it resolve `Done`.
/// Fired by the iterator's `return()` BEFORE its queued release step, so a
/// consumer can stop even while blocked waiting for the next event.
#[op2(fast)]
fn op_flow_events_cancel(state: &mut OpState) {
  if let Some(handle) = state.try_borrow::<EventsCancelHandle>() {
    handle.0.cancel();
  }
}

/// Returns the receiver to the relay task; inherit-mode draining resumes.
/// No-op when the stream is not claimed by this isolate.
#[op2(fast)]
fn op_flow_events_release(state: &mut OpState) {
  state.try_take::<EventsCancelHandle>();
  let Some(rx) =
    state.try_take::<mpsc::UnboundedReceiver<WorkerEventWithMetadata>>()
  else {
    return;
  };
  if let Some(cmd_tx) = state.try_borrow::<mpsc::UnboundedSender<RelayCmd>>() {
    let _ = cmd_tx.send(RelayCmd::Release(rx));
  }
}

deno_core::extension!(
  // flow: OPS-ONLY for the same reason as `user_workers_ops` - an ESM-bearing
  // extension can't link against Deno's CLI snapshot. The JS surface
  // (`FlowRuntime.events`) is installed post-bootstrap by flow_main.js, and
  // these op names must stay in the `NOT_IMPORTED_OPS` allowlist in
  // runtime/js/99_main.js.
  flow_events_ops,
  ops = [
    op_flow_events_claim,
    op_flow_events_accept,
    op_flow_events_cancel,
    op_flow_events_release,
  ],
);
