//! Worker lifecycle/log event types plus the console interceptor op.
//!
//! Lineage note: the edge-runtime "events worker" (a dedicated worker isolate
//! consuming these events) was removed in flow; events flow to the host
//! process instead (see `edge/cli/src/flow_events.rs`). The crate keeps its
//! name to ease merges from upstream.

pub mod events;
pub mod js_interceptors;
