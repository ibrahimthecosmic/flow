use std::collections::HashMap;

use deno_core::serde_json::Value;
use deno_facade::source_map_store;
use ext_event_worker::events::EventMetadata;
use ext_event_worker::events::WorkerEventWithMetadata;
use ext_event_worker::events::WorkerEvents;
use ext_workers::context::UserWorkerRuntimeOpts;
use once_cell::sync::Lazy;
use regex::Regex;
use tokio::sync::mpsc;

static VFS_PATH_REGEX: Lazy<Regex> =
  Lazy::new(|| Regex::new(r"file:///var/tmp/sb-compile-trex/").unwrap());

/// Apply source map translation to error messages
pub fn apply_source_maps(error_msg: &str) -> String {
  source_map_store::translate_error_locations(error_msg)
}

pub fn translate_vfs_paths(
  error_msg: &str,
  _service_path: Option<&str>,
) -> String {
  let replacement = match std::env::current_dir() {
    Ok(cwd) => format!(
      "file:///{}/",
      cwd
        .canonicalize()
        .unwrap_or(cwd)
        .to_string_lossy()
        .trim_start_matches('/')
        .trim_end_matches('/')
    ),
    Err(_) => return error_msg.to_string(),
  };
  VFS_PATH_REGEX
    .replace_all(error_msg, replacement.as_str())
    .to_string()
}

pub fn get_event_metadata(conf: &UserWorkerRuntimeOpts) -> EventMetadata {
  let mut otel_attributes = HashMap::new();
  let mut event_metadata = EventMetadata {
    service_path: conf.service_path.clone(),
    execution_id: conf.key,
    otel_attributes: None,
  };

  otel_attributes
    .insert("edge_runtime.worker.kind".to_string(), "user".to_string());

  let context = conf.context.clone().unwrap_or_default();
  if let Some(Value::Object(attributes)) = context.get("otel") {
    for (k, v) in attributes {
      otel_attributes.insert(
        k.to_string(),
        match v {
          Value::String(str) => str.to_string(),
          others => others.to_string(),
        },
      );
    }
  }

  event_metadata.otel_attributes = Some(otel_attributes);
  event_metadata
}

/// Forward a lifecycle/log event to the host-side events channel, when one
/// was wired up at pool standup.
pub fn send_event_if_event_worker_available(
  maybe_event_worker: Option<&mpsc::UnboundedSender<WorkerEventWithMetadata>>,
  event: WorkerEvents,
  metadata: EventMetadata,
) {
  if let Some(event_worker) = maybe_event_worker {
    let _ = event_worker.send(WorkerEventWithMetadata { event, metadata });
  }
}
