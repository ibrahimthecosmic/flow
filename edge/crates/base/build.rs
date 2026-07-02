use std::env;
use std::path::PathBuf;

// flow(2.9.0): startup-snapshot builder for the edge worker runtime.
//
// DEFAULT BUILD: a minimal EMPTY snapshot (no extensions) + empty residual
// tables. The real worker snapshot cannot be LOADED in a flow process (V8's
// process-shared read-only heap allows only one snapshot blob per process,
// and the main isolate's CLI_SNAPSHOT wins — see src/snapshot.rs for the
// full analysis), so baking the ~5 MiB blob by default would be dead weight.
//
// `FLOW_WORKER_SNAPSHOT=1`: bakes the REAL snapshot — the FULL worker
// extension set with every `esm` entry eagerly evaluated, so worker boots
// would skip extension-JS evaluation entirely (InitMode::FromSnapshot).
// Modeled on `runtime/snapshot.rs` + `cli/snapshot/build.rs` (which produce
// Deno's own CLI_SNAPSHOT): every extension is `lazy_init()` here
// (declarations + sources, no options/state); per-worker options/state are
// supplied at runtime by `src/runtime/mod.rs` via eager `init(args)` /
// `lazy_init_extensions`. VERIFIED to build; kept for experiments and for a
// future rusty_v8 `IsolateGroup` upgrade (per-group read-only heaps).
mod flow_startup_snapshot {
  use std::collections::HashSet;
  use std::io::Write;
  use std::rc::Rc;

  use deno_core::Extension;
  use deno_core::ExtensionFileSource;
  use deno_core::ExtensionFileSourceCode;
  use deno_core::snapshot::CreateSnapshotOptions;
  use deno_core::snapshot::create_snapshot;
  use deno_core::v8;

  use super::*;

  fn transpile_ts(
    specifier: deno_core::ModuleName,
    code: deno_core::ModuleCodeString,
  ) -> Result<
    (
      deno_core::ModuleCodeString,
      Option<deno_core::SourceMapData>,
    ),
    deno_error::JsErrorBox,
  > {
    deno::deno_runtime::transpile::maybe_transpile_source(specifier, code)
  }

  /// A `lazy_loaded_*` entry declared by an extension:
  /// `(specifier, source path, is_esm)`.
  struct LazyFile {
    specifier: String,
    path: PathBuf,
    is_esm: bool,
  }

  fn collect_lazy_extension_files(extensions: &[Extension]) -> Vec<LazyFile> {
    fn entry(file: &ExtensionFileSource, is_esm: bool) -> Option<LazyFile> {
      #[allow(deprecated, reason = "matching variant used by the ext macro")]
      let path = match &file.code {
        ExtensionFileSourceCode::LoadedFromFsDuringSnapshot(p) => {
          PathBuf::from(p)
        }
        _ => return None,
      };
      Some(LazyFile {
        specifier: file.specifier.to_string(),
        path,
        is_esm,
      })
    }

    let mut out = Vec::new();
    for ext in extensions {
      for file in &*ext.lazy_loaded_js_files {
        out.extend(entry(file, false));
      }
      for file in &*ext.lazy_loaded_esm_files {
        out.extend(entry(file, true));
      }
    }
    out.sort_by(|a, b| a.specifier.cmp(&b.specifier));
    out.dedup_by(|a, b| a.specifier == b.specifier);
    out
  }

  /// Pre-transpile a residual lazy source (runtime lazy loads skip the
  /// extension transpiler, so TS must become JS at build time).
  fn transpile_residual_source(
    out_dir: &std::path::Path,
    specifier: &str,
    src_path: &std::path::Path,
  ) -> PathBuf {
    let source = std::fs::read_to_string(src_path).unwrap_or_else(|e| {
      panic!(
        "failed to read residual lazy source {}: {e}",
        src_path.display()
      )
    });
    let name = deno_core::ModuleName::from(specifier.to_string());
    let (transpiled, _source_map) =
      transpile_ts(name, deno_core::ModuleCodeString::from(source))
        .unwrap_or_else(|e| {
          panic!("failed to transpile residual lazy source {specifier}: {e}")
        });

    let sanitized: String = specifier
      .chars()
      .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
      .collect();
    let out_path = out_dir.join(format!("{sanitized}.js"));
    std::fs::write(&out_path, transpiled.as_bytes()).unwrap();
    out_path
  }

  /// Wrap a residual `lazy_loaded_js` source the way `load_ext_script`
  /// expects (see cli/snapshot/build.rs).
  fn wrap_residual_js_source(path: &std::path::Path) {
    let source = std::fs::read_to_string(path).unwrap();
    let wrapped = deno_core::wrap_lazy_ext_script(&source);
    std::fs::write(path, wrapped.as_bytes()).unwrap();
  }

  fn write_residual_table(
    f: &mut std::fs::File,
    out_dir: &std::path::Path,
    name: &str,
    entries: &[(&str, PathBuf)],
  ) {
    writeln!(f, "pub static {name}: &[(&str, &str)] = &[").unwrap();
    for (specifier, transpiled_path) in entries {
      let rel = transpiled_path.strip_prefix(out_dir).unwrap();
      writeln!(
        f,
        "  ({specifier:?}, include_str!(concat!(env!(\"OUT_DIR\"), {:?}))),",
        format!("/{}", rel.display()),
      )
      .unwrap();
    }
    writeln!(f, "];\n").unwrap();
  }

  /// Default build: minimal empty snapshot + empty residual tables. See the
  /// module comment.
  pub fn create_empty_snapshot(snapshot_path: PathBuf, residual_path: PathBuf) {
    let output = create_snapshot(
      CreateSnapshotOptions {
        cargo_manifest_dir: env!("CARGO_MANIFEST_DIR"),
        startup_snapshot: None,
        extensions: vec![],
        extension_transpiler: Some(Rc::new(transpile_ts)),
        skip_op_registration: false,
        with_runtime_cb: None,
      },
      None,
    )
    .unwrap();

    let mut snapshot_file = std::fs::File::create(snapshot_path).unwrap();
    snapshot_file.write_all(&output.output).unwrap();

    let mut f = std::fs::File::create(&residual_path).unwrap();
    writeln!(
      f,
      "// @generated by edge/crates/base/build.rs - do not edit.\n\n\
       pub static RESIDUAL_LAZY_JS: &[(&str, &str)] = &[];\n\
       pub static RESIDUAL_LAZY_ESM: &[(&str, &str)] = &[];"
    )
    .unwrap();
  }

  pub fn create_runtime_snapshot(
    snapshot_path: PathBuf,
    residual_path: PathBuf,
  ) {
    // KEEP IN SYNC with the worker extension list in `src/runtime/mod.rs`
    // (`create_user_worker_pool` runtime build): same extensions, same ORDER
    // (op index layout). `base_runtime_permissions` is intentionally absent:
    // it is state-only (zero ops, zero JS, defined in this crate so it cannot
    // be referenced from the build script) and does not disturb the layout.
    let extensions: Vec<Extension> = vec![
      deno_telemetry::deno_telemetry::lazy_init(),
      deno_webidl::deno_webidl::lazy_init(),
      deno_web::deno_web::lazy_init(),
      deno_webgpu::deno_webgpu::lazy_init(),
      deno_image::deno_image::lazy_init(),
      deno_canvas::deno_canvas::lazy_init(),
      deno_fetch::deno_fetch::lazy_init(),
      deno_websocket::deno_websocket::lazy_init(),
      deno_crypto::deno_crypto::lazy_init(),
      deno_net::deno_net::lazy_init(),
      deno_tls::deno_tls::lazy_init(),
      deno_node_crypto::deno_node_crypto::lazy_init(),
      deno_node_sqlite::deno_node_sqlite::lazy_init(),
      deno_http::deno_http::lazy_init(),
      deno_io::deno_io::lazy_init(),
      deno_fs::deno_fs::lazy_init(),
      ext_ai::ai::lazy_init(),
      ext_env::env::lazy_init(),
      deno_process::deno_process::lazy_init(),
      ext_workers::user_workers::lazy_init(),
      ext_event_worker::user_event_worker::lazy_init(),
      ext_event_worker::js_interceptors::js_interceptors::lazy_init(),
      ext_runtime::runtime_bootstrap::lazy_init(),
      ext_runtime::runtime_net::lazy_init(),
      ext_runtime::runtime_http::lazy_init(),
      ext_runtime::runtime_http_start::lazy_init(),
      ext_node::deno_node::lazy_init::<
        deno_resolver::npm::DenoInNpmPackageChecker,
        deno_resolver::npm::NpmResolver<sys_traits::impls::RealSys>,
        sys_traits::impls::RealSys,
      >(),
      deno_cache::deno_cache::lazy_init(),
      deno::deno_runtime::ops::permissions::deno_permissions::lazy_init(),
      ext_os::os::lazy_init(),
      ext_os::deno_os::lazy_init(),
      ext_runtime::runtime::lazy_init(),
    ];

    let lazy_extension_files = collect_lazy_extension_files(&extensions);

    println!("Creating the flow worker snapshot...");
    let output = create_snapshot(
      CreateSnapshotOptions {
        cargo_manifest_dir: env!("CARGO_MANIFEST_DIR"),
        startup_snapshot: None,
        extensions,
        extension_transpiler: Some(Rc::new(transpile_ts)),
        skip_op_registration: false,
        // node:vm expects a pre-baked V8 context at VM_CONTEXT_INDEX when
        // booting from a snapshot (mirrors runtime/snapshot.rs).
        with_runtime_cb: Some(Box::new(|rt| {
          let isolate = rt.v8_isolate();
          v8::scope!(scope, isolate);

          let tmpl = ext_node::init_global_template(
            scope,
            ext_node::ContextInitMode::ForSnapshot,
          );
          let ctx = ext_node::create_v8_context(
            scope,
            tmpl,
            ext_node::ContextInitMode::ForSnapshot,
            std::ptr::null_mut(),
          );
          assert_eq!(scope.add_context(ctx), ext_node::VM_CONTEXT_INDEX);
        })),
      },
      None,
    )
    .unwrap();

    let mut snapshot_file = std::fs::File::create(snapshot_path).unwrap();
    snapshot_file.write_all(&output.output).unwrap();
    println!("Snapshot created successfully");

    for path in &output.files_loaded_during_snapshot {
      println!("cargo:rerun-if-changed={}", path.display());
    }

    // Emit the residual (not-consumed-into-the-snapshot) lazy sources table;
    // `src/snapshot.rs` includes it and `src/runtime/mod.rs` hands it to
    // `RuntimeOptions.residual_lazy_{js,esm}_sources`.
    let consumed: HashSet<&str> = output
      .consumed_lazy_specifiers
      .iter()
      .map(String::as_str)
      .collect();

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let residual_sources_dir = out_dir.join("residual_sources");
    std::fs::create_dir_all(&residual_sources_dir).unwrap();

    let mut residual_js: Vec<(&str, PathBuf)> = Vec::new();
    let mut residual_esm: Vec<(&str, PathBuf)> = Vec::new();
    for file in &lazy_extension_files {
      if consumed.contains(file.specifier.as_str()) {
        continue;
      }
      println!("cargo:rerun-if-changed={}", file.path.display());
      let transpiled_path = transpile_residual_source(
        &residual_sources_dir,
        &file.specifier,
        &file.path,
      );
      if file.is_esm {
        residual_esm.push((file.specifier.as_str(), transpiled_path));
      } else {
        wrap_residual_js_source(&transpiled_path);
        residual_js.push((file.specifier.as_str(), transpiled_path));
      }
    }

    let mut f = std::fs::File::create(&residual_path).unwrap();
    writeln!(
      f,
      "// @generated by edge/crates/base/build.rs - do not edit.\n"
    )
    .unwrap();
    write_residual_table(&mut f, &out_dir, "RESIDUAL_LAZY_JS", &residual_js);
    write_residual_table(&mut f, &out_dir, "RESIDUAL_LAZY_ESM", &residual_esm);
  }
}

fn main() {
  // Rebuild if build script changes
  println!("cargo:rerun-if-changed=build.rs");
  println!("cargo:rerun-if-env-changed=FLOW_WORKER_SNAPSHOT");

  println!("cargo:rustc-env=TARGET={}", env::var("TARGET").unwrap());
  println!("cargo:rustc-env=PROFILE={}", env::var("PROFILE").unwrap());

  let o = PathBuf::from(env::var_os("OUT_DIR").unwrap());
  let runtime_snapshot_path = o.join("RUNTIME_SNAPSHOT.bin");
  let residual_path = o.join("EXTENSION_RESIDUAL_SOURCES.rs");
  if env::var_os("FLOW_WORKER_SNAPSHOT").is_some() {
    flow_startup_snapshot::create_runtime_snapshot(
      runtime_snapshot_path,
      residual_path,
    );
  } else {
    flow_startup_snapshot::create_empty_snapshot(
      runtime_snapshot_path,
      residual_path,
    );
  }
}
