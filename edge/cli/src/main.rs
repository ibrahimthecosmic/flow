use std::ffi::OsStr;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Error;
use anyhow::bail;
use base::CacheSetting;
use base::Inspector;
use base::InspectorOption;
use base::InspectorServer;
use base::WorkerKind;
use base::get_default_permissions;
use base::worker::create_user_worker_pool;
use deno::deno_core::futures::FutureExt;
use deno_facade::DenoOptionsBuilder;
use deno_facade::EmitterFactory;
use deno_facade::Metadata;
use deno_facade::extract_from_file;
use deno_facade::generate_binary_eszip;
use flags::EszipV2ChecksumKind;
use flags::get_cli;
use tokio::time::timeout;

mod flags;
mod flow_config;
mod flow_events;

/// `flow` is a drop-in Deno binary plus the edge layer. Everything except the
/// flow-specific `eszip` subcommand group is delegated verbatim to the full
/// Deno CLI (`deno::main`). The `eszip` group lives here because building an
/// eszip needs `deno_facade`, which depends on the `deno` (cli) crate — so the
/// handler must sit *above* both (the cli crate itself cannot depend on
/// deno_facade without a dependency cycle).
fn main() -> ExitCode {
  let args: Vec<std::ffi::OsString> = std::env::args_os().collect();

  if args.get(1).map(|s| s.as_os_str()) != Some(OsStr::new("eszip")) {
    // Full Deno CLI plus the additive edge layer. `deno::main()` parses args
    // itself and exits the process, so this does not return.
    install_flow_embedding();
    deno::main();
    return ExitCode::SUCCESS;
  }

  let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
  env_logger::Builder::from_env(
    env_logger::Env::default().default_filter_or("info"),
  )
  .init();

  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .thread_name("flow-eszip")
    .build()
    .unwrap();

  let local = tokio::task::LocalSet::new();
  let res: Result<ExitCode, Error> = local.block_on(&runtime, run_eszip());

  match res {
    Ok(code) => code,
    Err(err) => {
      #[allow(clippy::print_stderr, reason = "top-level CLI error report")]
      {
        eprintln!("error: {err:#}");
      }
      ExitCode::FAILURE
    }
  }
}

/// Registers the flow embedding into Deno's main worker via the `deno::embed`
/// seam, before `deno::main()` runs. This is what turns plain Deno into flow:
/// every `flow run script.ts` gets the additive `FlowRuntime` host surface and
/// can spawn hardened user workers — while the main isolate stays a full,
/// unmodified Deno.
///
/// Two seams are used (both no-ops for plain Deno when unregistered):
///   1. an OPS-ONLY extension (`user_workers_ops`) carrying the user-worker ops.
///      It must be ops-only: an ESM-bearing extension can't be linked against
///      Deno's CLI snapshot and panics at init.
///   2. a post-bootstrap step that (a) stands up the user-worker pool on the
///      main worker's tokio runtime and puts its sender into op_state, then
///      (b) installs the `FlowRuntime` global by evaluating `flow_main.js`,
///      which calls the ops directly. It runs after `bootstrapMainRuntime`, so
///      `globalThis.Deno` and the snapshot `ext:` modules are already live.
fn install_flow_embedding() {
  // Surface flow's `eszip` subcommand group in `flow --help` (Deno's clap owns
  // the root help, whose fixed template has no subcommand slot). flow intercepts
  // `eszip` in `main()` before `deno::main()`, so this is help text only.
  deno::embed::register_help_section(Box::new(flags::flow_help_section));

  // Resolve the FLOW_* runtime env vars into the process-global cells that
  // flow's user workers consult (deprecation warnings, memcheck, default
  // user-worker heap). Independent of argv, so resolve it up front.
  flow_config::resolve_runtime_env_cells();

  // Strip flow's top-level pool/lifecycle flags out of argv before Deno's flag
  // parser runs, resolving them (over the FLOW_* env base) into the pool config
  // consumed by the post-bootstrap standup below.
  deno::embed::register_arg_filter(Box::new(flow_config::strip_and_resolve));

  // Share ONE cross-isolate store between the flow main isolate and all user
  // workers, so ArrayBuffers can be TRANSFERRED (zero-copy) over the
  // main<->worker MessagePorts (`port.postMessage(data, [buf])`) — the
  // raw-byte path of the comms surface.
  deno::embed::register_shared_array_buffer_store(
    ext_workers::FLOW_SHARED_ARRAY_BUFFER_STORE.clone(),
  );

  deno::embed::register_main_worker_extensions(Box::new(|| {
    vec![
      ext_workers::user_workers_ops::init(),
      flow_events::flow_events_ops::init(),
    ]
  }));

  deno::embed::register_main_worker_post_bootstrap(Box::new(|js_runtime| {
    // Stand up the user-worker pool on the current (main worker) tokio runtime.
    // `create_user_worker_pool` only spawns its loop task and returns, so the
    // future resolves synchronously — `now_or_never` is sound here.
    let flow_config = flow_config::FlowConfig::get();
    let (policy, server_flags) = flow_config.to_pool_config();

    // When user-worker debugging is enabled, stand up ONE shared inspector
    // server; every user worker registers itself as a distinct DevTools target
    // (see `op_user_worker_inspect`). This is separate from Deno's `--inspect`,
    // which still debugs the (pure Deno) main isolate.
    let inspector = flow_config.inspector_address().map(|addr| {
      let _ = ext_workers::USER_WORKER_INSPECTOR_HOST.set(addr);
      let server = Arc::new(InspectorServer::new(addr, "flow-user-workers"));
      Inspector::with_option(InspectorOption::Inspect(addr), server)
    });

    // Always-on user-worker event channel: the pool sends lifecycle events
    // (Boot/Shutdown/BootFailure/UncaughtException) and each worker's console
    // interceptor sends Log events here. Until user code claims
    // `FlowRuntime.events`, the relay task drains it with stdio-inherit
    // semantics; claiming hands the receiver over to the flow_events ops.
    let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
    let relay_cmd_tx = flow_events::spawn_events_relay(events_rx);

    let (_metric_src, worker_pool_tx) = create_user_worker_pool(
      Arc::new(server_flags),
      policy,
      Some(events_tx),
      None,
      vec![],
      inspector,
    )
    .now_or_never()
    .expect("user worker pool standup unexpectedly yielded")
    .expect("user worker pool standup failed");

    // The `op_user_worker_create` op borrows this sender from op_state. It must
    // be present before any user JS calls `FlowRuntime.userWorkers.create()`,
    // which can only happen after this step completes.
    js_runtime.op_state().borrow_mut().put(worker_pool_tx);
    // Same deal for the flow_events ops (`FlowRuntime.events` claim/release).
    js_runtime.op_state().borrow_mut().put(relay_cmd_tx);

    // Evaluated as ESM (not a classic script) so it can `import` the
    // snapshotted `ext:core/mod.js` to reach the ops — `Deno.core` is not on
    // the public namespace post-bootstrap.
    js_runtime.lazy_load_es_module_with_code(
      "ext:flow/flow_main.js",
      include_str!("flow_main.js"),
    )?;

    Ok(())
  }));
}

async fn run_eszip() -> Result<ExitCode, Error> {
  let matches = get_cli().get_matches();
  let Some(("eszip", eszip_matches)) = matches.subcommand() else {
    return Ok(ExitCode::FAILURE);
  };

  match eszip_matches.subcommand() {
    Some(("bundle", sub)) => bundle(sub).await,
    Some(("unbundle", sub)) => unbundle(sub).await,
    _ => Ok(ExitCode::FAILURE),
  }
}

async fn bundle(sub: &clap::ArgMatches) -> Result<ExitCode, Error> {
  let output_path = sub.get_one::<String>("output").cloned().unwrap();
  let static_patterns: Vec<String> = sub
    .get_many::<String>("static")
    .map(|vals| vals.map(|s| s.to_string()).collect())
    .unwrap_or_default();
  let timeout_dur = sub
    .get_one::<u64>("timeout")
    .cloned()
    .map(Duration::from_secs);

  let entrypoint_script_path =
    PathBuf::from(sub.get_one::<String>("entrypoint").cloned().unwrap());
  if !entrypoint_script_path.is_file() {
    bail!(
      "entrypoint path does not exist ({})",
      entrypoint_script_path.display()
    );
  }
  let entrypoint_script_path = entrypoint_script_path.canonicalize()?;

  let mut emitter_factory = EmitterFactory::new();
  if sub
    .get_one::<bool>("disable-module-cache")
    .copied()
    .unwrap_or(false)
  {
    emitter_factory.set_cache_strategy(Some(CacheSetting::ReloadAll));
  }

  let maybe_checksum_kind = sub
    .get_one::<EszipV2ChecksumKind>("checksum")
    .copied()
    .and_then(EszipV2ChecksumKind::into);

  emitter_factory.set_permissions_options(Some(get_default_permissions(
    WorkerKind::MainWorker,
  )));

  let builder =
    DenoOptionsBuilder::new().entrypoint(entrypoint_script_path.clone());
  emitter_factory.set_deno_options(builder.build().await?);

  let static_pattern_refs: Vec<&str> =
    static_patterns.iter().map(|s| s.as_str()).collect();
  let mut metadata = Metadata::default();
  #[allow(
    clippy::arc_with_non_send_sync,
    reason = "eszip generation runs on one thread; the Arc-wrapped factory never crosses threads"
  )]
  let eszip_fut = generate_binary_eszip(
    &mut metadata,
    Arc::new(emitter_factory),
    None,
    maybe_checksum_kind,
    Some(static_pattern_refs),
  );

  let eszip = if let Some(dur) = timeout_dur {
    match timeout(dur, eszip_fut).await {
      Ok(eszip) => eszip,
      Err(_) => bail!("Failed to complete the bundle within the given time."),
    }
  } else {
    eszip_fut.await
  }?;

  let bin = eszip.into_bytes();
  if output_path == "-" {
    std::io::stdout().lock().write_all(&bin)?;
  } else {
    File::create(output_path.as_str())?.write_all(&bin)?;
  }

  Ok(ExitCode::SUCCESS)
}

async fn unbundle(sub: &clap::ArgMatches) -> Result<ExitCode, Error> {
  let output_path =
    PathBuf::from(sub.get_one::<String>("output").cloned().unwrap());
  let eszip_path =
    PathBuf::from(sub.get_one::<String>("eszip").cloned().unwrap());

  if extract_from_file(eszip_path, output_path.clone()).await {
    #[allow(clippy::print_stdout, reason = "CLI success output")]
    {
      println!(
        "Eszip extracted successfully inside path {}",
        output_path.to_str().unwrap()
      );
    }
    Ok(ExitCode::SUCCESS)
  } else {
    Ok(ExitCode::FAILURE)
  }
}
