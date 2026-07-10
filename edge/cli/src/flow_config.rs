//! flow runtime configuration: the worker-pool / user-worker knobs that flow
//! layers on top of Deno.
//!
//! Two mechanisms feed this:
//!   1. `FLOW_*` environment variables (the base layer), and
//!   2. flow-specific CLI flags (which override the env), stripped from argv by
//!      [`strip_and_resolve`] before Deno's flag parser ever sees them.
//!
//! Only knobs that are actually live in flow's architecture are exposed. flow
//! runs the user-worker pool directly and does NOT run edge's HTTP server or
//! create main/event workers, so every HTTP-serving flag and every main/event
//! worker flag from edge is intentionally absent (they would be dead config).

#![allow(
  clippy::print_stderr,
  reason = "flag/env parsing warnings surface on stderr before any logger exists"
)]

use std::ffi::OsString;
use std::net::SocketAddr;
use std::sync::OnceLock;

use base::flags::WorkerFlags;
use base::worker::pool::SupervisorPolicy;
use base::worker::pool::WorkerPoolPolicy;

/// The resolved flow pool/lifecycle configuration, stored once at startup and
/// consumed when the user-worker pool is stood up post-bootstrap.
#[derive(Debug, Default, Clone)]
pub(crate) struct FlowConfig {
  policy: Option<SupervisorPolicy>,
  max_parallelism: Option<usize>,
  request_wait_timeout_ms: Option<u64>,
  beforeunload_wall_clock_pct: Option<u8>,
  beforeunload_cpu_pct: Option<u8>,
  beforeunload_memory_pct: Option<u8>,
  /// Address to bind the shared user-worker inspector server to. `Some` enables
  /// per-user-worker debugging (and `worker.inspect()`); `None` disables it.
  /// This is distinct from Deno's own `--inspect` (which debugs flow's main
  /// isolate, untouched) — flow's main isolate stays pure Deno.
  inspector_address: Option<SocketAddr>,
}

static FLOW_CONFIG: OnceLock<FlowConfig> = OnceLock::new();

impl FlowConfig {
  /// The resolved config, or defaults if startup resolution never ran (e.g. a
  /// code path that bypassed `strip_and_resolve`).
  pub(crate) fn get() -> FlowConfig {
    FLOW_CONFIG.get().cloned().unwrap_or_default()
  }

  /// The user-worker inspector bind address, if enabled.
  pub(crate) fn inspector_address(&self) -> Option<SocketAddr> {
    self.inspector_address
  }

  /// Build the pool policy + the (worker-lifecycle) server flags the pool and
  /// user-worker supervisor consume. `request_wait_timeout_ms` is threaded via
  /// `WorkerFlags` because `WorkerPoolPolicy::new` reads it from there.
  pub(crate) fn to_pool_config(&self) -> (WorkerPoolPolicy, WorkerFlags) {
    let server_flags = WorkerFlags {
      request_wait_timeout_ms: self.request_wait_timeout_ms,
      beforeunload_wall_clock_pct: self.beforeunload_wall_clock_pct,
      beforeunload_cpu_pct: self.beforeunload_cpu_pct,
      beforeunload_memory_pct: self.beforeunload_memory_pct,
      ..Default::default()
    };
    let policy =
      WorkerPoolPolicy::new(self.policy, self.max_parallelism, server_flags);
    (policy, server_flags)
  }

  /// Seed a config from the `FLOW_*` environment variables. CLI flags layered
  /// on top of this override individual fields.
  fn from_env() -> Self {
    Self {
      policy: env_var("FLOW_WORKER_POOL_POLICY").and_then(|v| parse_policy(&v)),
      max_parallelism: env_parse("FLOW_WORKER_MAX_PARALLELISM"),
      request_wait_timeout_ms: env_parse("FLOW_REQUEST_WAIT_TIMEOUT_MS"),
      beforeunload_wall_clock_pct: env_parse_ratio(
        "FLOW_BEFOREUNLOAD_WALL_CLOCK_RATIO",
      ),
      beforeunload_cpu_pct: env_parse_ratio("FLOW_BEFOREUNLOAD_CPU_RATIO"),
      beforeunload_memory_pct: env_parse_ratio(
        "FLOW_BEFOREUNLOAD_MEMORY_RATIO",
      ),
      inspector_address: env_var("FLOW_USER_WORKER_INSPECTOR_ADDRESS")
        .and_then(|v| {
          parse_socket_addr("FLOW_USER_WORKER_INSPECTOR_ADDRESS", &v)
        }),
    }
  }

  /// Apply a single `--flag`/value pair, overriding the env-seeded field.
  /// Unknown or malformed values are warned about and skipped (lenient — env
  /// or default still applies).
  fn apply(&mut self, flag: &str, value: &str) {
    match flag {
      "--policy" => {
        if let Some(p) = parse_policy(value) {
          self.policy = Some(p);
        }
      }
      "--max-parallelism" => {
        self.max_parallelism = parse_or_warn(flag, value);
      }
      "--request-wait-timeout" => {
        self.request_wait_timeout_ms = parse_or_warn(flag, value);
      }
      "--dispatch-beforeunload-wall-clock-ratio" => {
        self.beforeunload_wall_clock_pct = parse_ratio_or_warn(flag, value);
      }
      "--dispatch-beforeunload-cpu-ratio" => {
        self.beforeunload_cpu_pct = parse_ratio_or_warn(flag, value);
      }
      "--dispatch-beforeunload-memory-ratio" => {
        self.beforeunload_memory_pct = parse_ratio_or_warn(flag, value);
      }
      "--user-worker-inspect" => {
        self.inspector_address = parse_socket_addr(flag, value);
      }
      _ => unreachable!("unhandled flow flag {flag}"),
    }
  }
}

/// The flow flags recognized on the command line. Each takes exactly one value
/// (either `--flag value` or `--flag=value`).
const FLOW_FLAGS: &[&str] = &[
  "--policy",
  "--max-parallelism",
  "--request-wait-timeout",
  "--dispatch-beforeunload-wall-clock-ratio",
  "--dispatch-beforeunload-cpu-ratio",
  "--dispatch-beforeunload-memory-ratio",
  "--user-worker-inspect",
];

/// Pull flow's flags out of `args`, resolve the full [`FlowConfig`] (env as the
/// base layer, CLI flags overriding), stash it for the post-bootstrap pool
/// standup, and return `args` with flow's flags removed so Deno's flag parser
/// only sees Deno flags.
///
/// flow flags are top-level and are only stripped up to a bare `--` terminator;
/// anything after `--` is a script argument and is passed through untouched.
pub(crate) fn strip_and_resolve(args: Vec<OsString>) -> Vec<OsString> {
  let mut config = FlowConfig::from_env();
  let mut out = Vec::with_capacity(args.len());
  let mut iter = args.into_iter().peekable();

  // arg0 always passes through.
  if let Some(arg0) = iter.next() {
    out.push(arg0);
  }

  let mut passthrough = false;
  while let Some(arg) = iter.next() {
    if passthrough {
      out.push(arg);
      continue;
    }

    let Some(s) = arg.to_str() else {
      out.push(arg);
      continue;
    };

    if s == "--" {
      passthrough = true;
      out.push(arg);
      continue;
    }

    // `--flag=value` form.
    if let Some((flag, value)) = s.split_once('=') {
      if FLOW_FLAGS.contains(&flag) {
        config.apply(flag, value);
        continue;
      }
    }

    // `--flag value` form: consume the following token as the value.
    if FLOW_FLAGS.contains(&s) {
      match iter.next() {
        Some(value_os) => match value_os.to_str() {
          Some(value) => config.apply(s, value),
          None => eprintln!("flow: ignoring non-UTF-8 value for {s}"),
        },
        None => eprintln!("flow: missing value for {s}"),
      }
      continue;
    }

    out.push(arg);
  }

  let _ = FLOW_CONFIG.set(config);
  out
}

/// Resolve the `FLOW_*` / `DENO_*` runtime env vars into the process-global
/// cells that flow's user workers consult. Call once at startup, before any
/// user worker is created. This is the flow replacement for edge's deleted
/// `env.rs`: the main/event-worker heap vars are dropped (those workers don't
/// exist in flow), and edge's `EDGE_RUNTIME_*` names are rebranded to `FLOW_*`.
pub(crate) fn resolve_runtime_env_cells() {
  use base::runtime;

  if let Some(v) = env_bool("DENO_NO_DEPRECATION_WARNINGS") {
    let _ = runtime::SHOULD_DISABLE_DEPRECATED_API_WARNING.set(v);
  }
  if let Some(v) = env_bool("DENO_VERBOSE_WARNINGS") {
    let _ = runtime::SHOULD_USE_VERBOSE_DEPRECATED_API_WARNING.set(v);
  }
  if let Some(v) = env_bool("FLOW_INCLUDE_MALLOCED_MEMORY_ON_MEMCHECK") {
    let _ = runtime::SHOULD_INCLUDE_MALLOCED_MEMORY_ON_MEMCHECK.set(v);
  }

  // Repurposed from edge's (inert-in-flow) main/event-worker heap vars: sets
  // the default user-worker memory limit when `create()` omits `memoryLimitMb`.
  if let Some(v) = env_parse::<u64>("FLOW_USER_WORKER_MAX_HEAP_SIZE_MIB") {
    let _ = ext_workers::USER_WORKER_DEFAULT_MEMORY_LIMIT_MIB.set(v);
  }
}

fn env_var(key: &str) -> Option<String> {
  std::env::var(key).ok().filter(|s| !s.is_empty())
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
  let raw = env_var(key)?;
  match raw.parse() {
    Ok(v) => Some(v),
    Err(_) => {
      eprintln!("flow: ignoring invalid value for {key}: {raw:?}");
      None
    }
  }
}

fn env_parse_ratio(key: &str) -> Option<u8> {
  env_parse::<u8>(key).and_then(|v| check_ratio(key, v))
}

fn env_bool(key: &str) -> Option<bool> {
  let raw = env_var(key)?.to_ascii_lowercase();
  Some(matches!(raw.as_str(), "1" | "true" | "yes" | "on"))
}

fn parse_policy(value: &str) -> Option<SupervisorPolicy> {
  match value {
    "per_worker" => Some(SupervisorPolicy::PerWorker),
    "per_request" => Some(SupervisorPolicy::PerRequest { oneshot: false }),
    "oneshot" => Some(SupervisorPolicy::PerRequest { oneshot: true }),
    other => {
      eprintln!(
        "flow: ignoring invalid --policy/FLOW_WORKER_POOL_POLICY value \
         {other:?} (expected per_worker, per_request, or oneshot)"
      );
      None
    }
  }
}

fn parse_or_warn<T: std::str::FromStr>(flag: &str, value: &str) -> Option<T> {
  match value.parse() {
    Ok(v) => Some(v),
    Err(_) => {
      eprintln!("flow: ignoring invalid value for {flag}: {value:?}");
      None
    }
  }
}

fn parse_ratio_or_warn(flag: &str, value: &str) -> Option<u8> {
  parse_or_warn::<u8>(flag, value).and_then(|v| check_ratio(flag, v))
}

fn parse_socket_addr(what: &str, value: &str) -> Option<SocketAddr> {
  match value.parse() {
    Ok(addr) => Some(addr),
    Err(_) => {
      eprintln!(
        "flow: ignoring invalid address for {what}: {value:?} \
         (expected host:port, e.g. 127.0.0.1:9229)"
      );
      None
    }
  }
}

fn check_ratio(what: &str, v: u8) -> Option<u8> {
  if v <= 99 {
    Some(v)
  } else {
    eprintln!(
      "flow: ignoring out-of-range value for {what}: {v} (expected 0-99)"
    );
    None
  }
}
