// flow(2.9.0): the worker startup snapshot is DISABLED — but for a precise,
// verified reason (NOT the vague trex "ARM64 SIGBUS" note this file used to
// carry):
//
// V8 149 (this rusty_v8 build) shares ONE read-only heap across every isolate
// in the process (single isolate group). The FIRST isolate to boot pins the
// RO-heap layout from its snapshot blob; every later blob must have the
// exact same RO layout or its RO references misresolve. flow's main isolate
// boots from Deno's CLI_SNAPSHOT (see cli/factory.rs — worth far more to
// startup than worker boots), so a *different* worker blob deserializes
// against the wrong RO layout and crashes:
//   - with the CLI snapshot loaded first: SIGSEGV in
//     SharedHeapDeserializer::DeserializeStringTable
//   - alone (no CLI snapshot): libc++ "vector[] index out of bounds" in
//     Deserializer::ReadReadOnlyHeapRef
// Effectively: ONE process, ONE snapshot blob. (This is very likely what
// trex actually hit.)
//
// The snapshot BUILDER (build.rs) is fully working — set
// `FLOW_WORKER_SNAPSHOT=1` at build time to bake the real blob (~5 MiB) for
// experiments. Revisit when rusty_v8 exposes v8::IsolateGroup (per-group RO
// heaps would let worker isolates load their own blob), or if flow ever
// moves the worker pool out of the CLI process.
pub static CLI_SNAPSHOT: &[u8] =
  include_bytes!(concat!(env!("OUT_DIR"), "/RUNTIME_SNAPSHOT.bin"));

/// `(specifier, source)` pairs for every `lazy_loaded_js` / `lazy_loaded_esm`
/// extension file NOT consumed into the snapshot. Empty in the default
/// (snapshot-disabled) build; populated when `FLOW_WORKER_SNAPSHOT=1` bakes
/// the real blob.
mod residual {
  include!(concat!(env!("OUT_DIR"), "/EXTENSION_RESIDUAL_SOURCES.rs"));
}

pub use residual::RESIDUAL_LAZY_ESM;
pub use residual::RESIDUAL_LAZY_JS;

pub fn snapshot() -> Option<&'static [u8]> {
  // See the module comment: loading this blob alongside the main isolate's
  // CLI_SNAPSHOT crashes V8 (shared read-only heap, one blob per process).
  None
}
