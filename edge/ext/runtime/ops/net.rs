use deno_core::OpState;
use deno_core::op2;

// User workers have no inbound network surface: the host<->worker comms run
// over MessagePorts, and flow has no HTTP ingress into workers. Every listen
// variant is denied at the op layer so the sandbox posture is explicit no
// matter which JS API (Deno.listen, node:http, ...) tries to bind a socket.
// Outbound networking (fetch, Deno.connect, ...) is untouched.
deno_core::extension!(
  runtime_net,
  middleware = |op| match op.name {
    "op_net_listen_tcp"
    | "op_net_accept_tcp"
    | "op_net_listen_tls"
    | "op_net_listen_udp"
    | "op_node_unstable_net_listen_udp"
    | "op_net_listen_unix"
    | "op_net_listen_unixpacket"
    | "op_node_unstable_net_listen_unixpacket" =>
      op.with_implementation_from(&op_net_unsupported()),
    _ => op,
  }
);

// The error class must be one the worker registers via
// `core.registerErrorClass` (see js/errors.js). An unregistered class (the
// old `RuntimeError::Runtime` mapped to "Runtime") makes deno_core's
// buildCustomError return undefined, so JS callers saw `throw undefined`
// instead of an error.
#[op2(fast)]
pub fn op_net_unsupported(
  _state: &mut OpState,
) -> Result<(), deno_error::JsErrorBox> {
  Err(deno_error::JsErrorBox::new(
    "NotSupported",
    "Listening on network sockets is not supported in flow user workers",
  ))
}
