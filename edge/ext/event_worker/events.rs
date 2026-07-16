use std::collections::HashMap;

use base_mem_check::MemCheckState;
use enum_as_inner::EnumAsInner;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

#[derive(Serialize, Deserialize, Debug)]
pub struct BootEvent {
  pub boot_time: usize,
}
#[derive(Serialize, Deserialize, Debug)]
pub struct BootFailureEvent {
  pub msg: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct WorkerMemoryUsed {
  pub total: usize,
  pub heap: usize,
  pub external: usize,
  pub mem_check_captured: MemCheckState,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum ShutdownReason {
  EventLoopCompleted,
  WallClockTime,
  CPUTime,
  Memory,
  EarlyDrop,
  TerminationRequested,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ShutdownEvent {
  pub reason: ShutdownReason,
  pub cpu_time_used: usize,
  pub memory_used: WorkerMemoryUsed,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UncaughtExceptionEvent {
  pub exception: String,
  pub cpu_time_used: usize,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct LogEvent {
  pub msg: String,
  pub level: LogLevel,
}

#[derive(Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum LogLevel {
  #[default]
  Debug,
  Info,
  Warning,
  Error,
}

impl From<u8> for LogLevel {
  fn from(value: u8) -> Self {
    match value {
      0 => Self::Debug,
      1 => Self::Info,
      2 => Self::Warning,
      3 => Self::Error,
      _ => Self::Debug,
    }
  }
}

/// flow: runtime-global bundle-cache activity (LRU/TTL eviction, explicit
/// evict, over-cap admission) relayed on the same stream as worker events.
/// Not tied to a worker — its `metadata` is empty. `action` is one of
/// `"evicted"`, `"overCap"`, `"sweep"`.
#[derive(Serialize, Deserialize, Debug)]
pub struct BundleCacheEvent {
  pub action: String,
  /// The manifest key, when the action targeted one (explicit evict).
  pub cache_key: Option<String>,
  /// The blob file involved, when the action targeted one.
  pub path: Option<String>,
  /// Bytes the action acted on (evicted/swept bytes; the incoming bundle
  /// size for `overCap`).
  pub bytes: u64,
  /// Cache total after the action.
  pub total_bytes: u64,
  /// The configured cap, when one is set.
  pub max_bytes: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, EnumAsInner)]
pub enum WorkerEvents {
  Boot(BootEvent),
  BootFailure(BootFailureEvent),
  UncaughtException(UncaughtExceptionEvent),
  Shutdown(ShutdownEvent),
  Log(LogEvent),
  /// flow: bundle-cache activity (see [`BundleCacheEvent`]).
  BundleCache(BundleCacheEvent),
}

impl WorkerEvents {
  pub fn with_cpu_time_used(mut self, cpu_time_used_ms: usize) -> Self {
    match &mut self {
      Self::UncaughtException(UncaughtExceptionEvent {
        cpu_time_used, ..
      })
      | Self::Shutdown(ShutdownEvent { cpu_time_used, .. }) => {
        *cpu_time_used = cpu_time_used_ms;
      }

      _ => {}
    }

    self
  }
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct EventMetadata {
  pub service_path: Option<String>,
  pub execution_id: Option<Uuid>,
  pub otel_attributes: Option<HashMap<String, String>>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct WorkerEventWithMetadata {
  pub event: WorkerEvents,
  pub metadata: EventMetadata,
}

#[derive(Serialize, Deserialize)]
pub enum RawEvent {
  Event(Box<WorkerEventWithMetadata>),
  Done,
}
