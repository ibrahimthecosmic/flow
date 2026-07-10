/// Worker-pool / user-worker lifecycle knobs.
///
/// Lineage note: this is what survives of edge-runtime's `ServerFlags` — flow
/// has no HTTP server, so every HTTP-serving knob is gone and only the
/// pool/worker settings remain (fed from flow's CLI flags / `FLOW_*` env, see
/// `edge/cli/src/flow_config.rs`).
#[derive(Debug, Default, Clone, Copy)]
pub struct WorkerFlags {
  /// How long a `create()` waits for a pool slot before failing.
  pub request_wait_timeout_ms: Option<u64>,

  pub beforeunload_wall_clock_pct: Option<u8>,
  pub beforeunload_cpu_pct: Option<u8>,
  pub beforeunload_memory_pct: Option<u8>,

  /// Deny `allowHostFsAccess` workers outright (embedder hardening knob;
  /// not exposed on the flow CLI).
  pub restrict_host_fs: bool,
}
