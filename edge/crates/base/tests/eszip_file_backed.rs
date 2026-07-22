//! Integration tests for file-backed eszip bundles (`EszipPayloadKind::
//! FileKind` + the content-addressed bundle cache).
//!
//! The memory-mode behavior is covered by the existing suites (every
//! `VecKind`/`JsBufferKind` user-worker payload now converges file-backed, so
//! `integration_tests.rs` and the runtime unit tests exercise the file path
//! end to end); this file covers what's new: the cache, the shared-bundle
//! registry, checksum validation on preads, old-format rejection, ranged
//! reads, and the RSS/cold-start characteristics the redesign exists for.

#![allow(
  clippy::print_stdout,
  reason = "test harness; RSS/cold-start figures are printed for tuning"
)]

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use base::flags::WorkerFlags;
use base::utils::test_utils::test_user_runtime_opts;
use base::worker::TerminationToken;
use base::worker::WorkerSurface;
use base::worker::WorkerSurfaceBuilder;
use deno_core::FastString;
use deno_facade::Checksum;
use deno_facade::DenoOptionsBuilder;
use deno_facade::EmitterFactory;
use deno_facade::EszipEntryReader;
use deno_facade::EszipPayloadKind;
use deno_facade::Metadata;
use deno_facade::bundle_cache;
use deno_facade::generate_binary_eszip;
use deno_facade::payload_to_eszip;
use either::Either;
use eszip_trait::AsyncEszipDataRead;
use ext_event_worker::events::LogLevel;
use ext_event_worker::events::WorkerEventWithMetadata;
use ext_event_worker::events::WorkerEvents;
use ext_workers::context::WorkerContextInitOpts;
use serial_test::serial;
use tokio::sync::mpsc;
use tokio::time::timeout;
use uuid::Uuid;

const EVENT_DEADLINE: Duration = Duration::from_secs(60);

fn fixture(path: &str) -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)
}

/// Bundles `module_code` (the `/src/index.ts` code-only form) into an eszip,
/// optionally checksummed and padded with `pad_mib` MiB of opaque entries to
/// simulate a large bundle.
async fn generate_test_eszip(
  module_code: &str,
  maybe_checksum: Option<Checksum>,
  pad_mib: usize,
) -> eszip::EszipV2 {
  let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

  let mut emitter_factory = EmitterFactory::new();
  emitter_factory
    .set_deno_options(DenoOptionsBuilder::new().build().await.unwrap());

  let mut metadata = Metadata::default();
  #[allow(
    clippy::arc_with_non_send_sync,
    reason = "single-threaded test; the Arc-wrapped factory never crosses threads"
  )]
  let mut eszip = generate_binary_eszip(
    &mut metadata,
    Arc::new(emitter_factory),
    Some(FastString::from(module_code.to_string())),
    maybe_checksum,
    None,
    None,
    None,
  )
  .await
  .unwrap();

  for i in 0..pad_mib {
    // Distinct first bytes so identical-content dedup can't shrink the pad.
    let mut blob = vec![0xAB_u8; 1 << 20];
    blob[..8].copy_from_slice(&(i as u64).to_be_bytes());
    eszip.add_opaque_data(format!("pad://{i}"), Arc::from(blob));
  }

  eszip
}

async fn generate_test_eszip_bytes(
  module_code: &str,
  maybe_checksum: Option<Checksum>,
  pad_mib: usize,
) -> Vec<u8> {
  generate_test_eszip(module_code, maybe_checksum, pad_mib)
    .await
    .into_bytes()
}

fn base_init_opts(eszip: EszipPayloadKind) -> WorkerContextInitOpts {
  WorkerContextInitOpts {
    service_path: fixture("test_cases"),
    no_module_cache: false,
    no_npm: None,
    env_vars: HashMap::new(),
    timing: None,
    maybe_eszip: Some(eszip),
    maybe_entrypoint: None,
    maybe_module_code: None,
    conf: Box::new(test_user_runtime_opts()),
    static_patterns: vec![],
    maybe_s3_fs_config: None,
    maybe_tmp_fs_config: None,
    maybe_http_fs_config: None,
    maybe_otel_config: None,
  }
}

struct EszipWorker {
  #[allow(dead_code, reason = "keeps the worker surface alive")]
  surface: WorkerSurface,
  termination_token: TerminationToken,
  events_rx: mpsc::UnboundedReceiver<WorkerEventWithMetadata>,
}

impl EszipWorker {
  /// Boots a user worker straight from an eszip payload with eager module
  /// init, so module-load failures (e.g. checksum mismatches) surface here.
  async fn try_boot(
    eszip: EszipPayloadKind,
  ) -> Result<EszipWorker, anyhow::Error> {
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let termination_token = TerminationToken::new();

    let mut opts = base_init_opts(eszip);
    opts.conf.key = Some(Uuid::new_v4());
    opts.conf.service_path =
      Some(opts.service_path.to_string_lossy().into_owned());
    opts.conf.events_msg_tx = Some(events_tx);

    let surface = WorkerSurfaceBuilder::new()
      .init_opts(opts)
      .worker_flags(Either::Right(WorkerFlags::default()))
      .termination_token(termination_token.clone())
      .eager_module_init(true)
      .build()
      .await?;

    Ok(EszipWorker {
      surface,
      termination_token,
      events_rx,
    })
  }

  async fn expect_log_containing(&mut self, needle: &str) {
    let fut = async {
      while let Some(ev) = self.events_rx.recv().await {
        if matches!(
          &ev.event,
          WorkerEvents::Log(log)
            if log.level == LogLevel::Info && log.msg.contains(needle)
        ) {
          return true;
        }
      }
      false
    };

    match timeout(EVENT_DEADLINE, fut).await {
      Ok(true) => {}
      Ok(false) => panic!("events channel closed before a log with {needle:?}"),
      Err(_) => panic!("timed out waiting for a log with {needle:?}"),
    }
  }

  async fn terminate(&self) {
    let fut = self.termination_token.cancel_and_wait();
    if timeout(Duration::from_secs(30), fut).await.is_err() {
      panic!("worker did not terminate in time");
    }
  }
}

/// Points the bundle cache at a fresh temp dir for the duration of a test.
struct CacheDirGuard {
  #[allow(dead_code, reason = "removes the dir on drop")]
  dir: tempfile::TempDir,
}

impl CacheDirGuard {
  fn new() -> Self {
    let dir = tempfile::Builder::new()
      .prefix("flow-bundle-cache-test")
      .tempdir()
      .unwrap();
    // SAFETY: tests run under `#[serial]`; no other thread reads the env
    // concurrently.
    unsafe { std::env::set_var("FLOW_BUNDLE_CACHE_DIR", dir.path()) };
    Self { dir }
  }

  fn path(&self) -> &Path {
    self.dir.path()
  }

  fn cached_bundles(&self) -> Vec<PathBuf> {
    std::fs::read_dir(self.path())
      .map(|entries| {
        entries
          .flatten()
          .map(|it| it.path())
          .filter(|it| it.extension().is_some_and(|ext| ext == "eszip"))
          .collect()
      })
      .unwrap_or_default()
  }
}

impl Drop for CacheDirGuard {
  fn drop(&mut self) {
    // SAFETY: tests run under `#[serial]`; no other thread reads the env
    // concurrently.
    unsafe { std::env::remove_var("FLOW_BUNDLE_CACHE_DIR") };
  }
}

#[cfg(target_os = "linux")]
fn vm_rss_kib() -> u64 {
  let status = std::fs::read_to_string("/proc/self/status").unwrap();
  status
    .lines()
    .find_map(|line| {
      line
        .strip_prefix("VmRSS:")
        .and_then(|rest| rest.trim().strip_suffix("kB"))
        .and_then(|kib| kib.trim().parse().ok())
    })
    .expect("VmRSS not found in /proc/self/status")
}

// ---------------------------------------------------------------------------
// Correctness
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_file_backed_path_input_boots_and_serves() {
  let bytes = generate_test_eszip_bytes(
    "console.log('file-backed path input works');",
    None,
    0,
  )
  .await;

  let dir = tempfile::tempdir().unwrap();
  let bundle_path = dir.path().join("bundle.eszip");
  std::fs::write(&bundle_path, &bytes).unwrap();
  drop(bytes);

  let mut worker =
    EszipWorker::try_boot(EszipPayloadKind::FileKind(bundle_path))
      .await
      .unwrap();
  worker
    .expect_log_containing("file-backed path input works")
    .await;
  worker.terminate().await;
}

#[tokio::test]
#[serial]
async fn test_buffer_input_lands_in_cache_and_boots() {
  let cache = CacheDirGuard::new();
  let bytes = generate_test_eszip_bytes(
    "console.log('file-backed buffer input works');",
    None,
    0,
  )
  .await;

  let mut worker = EszipWorker::try_boot(EszipPayloadKind::VecKind(bytes))
    .await
    .unwrap();
  worker
    .expect_log_containing("file-backed buffer input works")
    .await;
  worker.terminate().await;

  let cached = cache.cached_bundles();
  assert_eq!(
    cached.len(),
    1,
    "the buffer payload should have spilled into the bundle cache (got: \
     {cached:?})"
  );
}

#[tokio::test]
#[serial]
async fn test_identical_buffers_converge_on_one_cache_file() {
  let cache = CacheDirGuard::new();
  let bytes = b"not a real eszip; the cache is content-addressed".to_vec();

  let first = bundle_cache::store_bytes(bytes.clone()).await.unwrap();
  let second = bundle_cache::store_bytes(bytes).await.unwrap();

  assert_eq!(first, second);
  assert_eq!(cache.cached_bundles().len(), 1);
}

#[tokio::test]
#[serial]
async fn test_concurrent_creates_share_one_parsed_bundle() {
  let bytes = generate_test_eszip_bytes(
    "console.log('shared bundle registry works');",
    None,
    0,
  )
  .await;
  let dir = tempfile::tempdir().unwrap();
  let bundle_path = dir.path().join("bundle.eszip");
  std::fs::write(&bundle_path, &bytes).unwrap();

  let opens = (0..8).map(|_| {
    let path = bundle_path.clone();
    tokio::spawn(async move { bundle_cache::open_shared(&path).await })
  });
  let bundles = futures_util::future::try_join_all(opens)
    .await
    .unwrap()
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .unwrap();

  assert!(
    bundles
      .iter()
      .all(|bundle| Arc::ptr_eq(bundle, &bundles[0])),
    "concurrent creates of the same path must share one parsed bundle"
  );
}

#[tokio::test]
#[serial]
async fn test_v0_eszip_rejected_with_rebundle_error() {
  let v0_path = fixture("test_cases/eszip-migration/npm-axios/v0.eszip");

  let result =
    EszipWorker::try_boot(EszipPayloadKind::FileKind(v0_path.clone())).await;
  let err = format!("{:#}", result.err().expect("v0 boot must fail"));
  assert!(
    err.contains("re-bundle"),
    "old formats must be rejected with an actionable error (got: {err})"
  );

  // The migration machinery is untouched for the unbundle tooling: the same
  // archive still opens (and migrates) through `EszipEntryReader`.
  let bytes = std::fs::read(&v0_path).unwrap();
  let reader = EszipEntryReader::open(EszipPayloadKind::VecKind(bytes))
    .await
    .expect("EszipEntryReader must still migrate v0 archives");
  assert!(reader.remaining() > 0);
}

#[tokio::test]
#[serial]
async fn test_checksum_tamper_fails_module_load() {
  const MARKER: &str = "FLOW_TAMPER_MARKER_0123456789";

  let code = format!(
    "const marker = \"{MARKER}\";\nconsole.log('never reached', marker);"
  );
  let mut bytes =
    generate_test_eszip_bytes(&code, Some(Checksum::XxHash3), 0).await;

  // Flip one byte inside the entry module's source extent (found via the
  // unique marker the source embeds).
  let marker_pos = bytes
    .windows(MARKER.len())
    .position(|w| w == MARKER.as_bytes())
    .expect("the marker must appear in the emitted source");
  bytes[marker_pos] ^= 0x01;

  let dir = tempfile::tempdir().unwrap();
  let bundle_path = dir.path().join("tampered.eszip");
  std::fs::write(&bundle_path, &bytes).unwrap();

  // The read the import performs (the loader goes through `read_source`)
  // must reject the flipped byte...
  let probe = payload_to_eszip(EszipPayloadKind::FileKind(bundle_path.clone()))
    .await
    .unwrap();
  let read_err = probe
    .read_source("file:///src/index.ts")
    .await
    .expect_err("a flipped source byte must fail the checksum");
  assert!(
    read_err.to_string().contains("invalid source hash"),
    "unexpected read error: {read_err}"
  );
  drop(probe);

  // ...and a worker booted from the tampered bundle must die during eager
  // module init with that same error (the boot itself succeeds; module init
  // happens on the worker thread).
  let mut worker =
    EszipWorker::try_boot(EszipPayloadKind::FileKind(bundle_path))
      .await
      .unwrap();
  let handles = worker
    .surface
    .thread_handles
    .lock()
    .unwrap()
    .take()
    .expect("worker thread handles must be present");
  let joined =
    tokio::task::spawn_blocking(move || handles.thread_handle.join())
      .await
      .unwrap()
      .expect("worker thread must not panic");
  let err = format!("{:#}", joined.expect_err("module init must fail"));
  assert!(
    err.contains("invalid source hash"),
    "a flipped source byte must fail module init with a checksum error \
     (got: {err})"
  );

  // The failure must also reach the host's events channel (that's the only
  // signal `FlowRuntime.userWorkers.create` callers get, since the boot
  // signal resolves before module init).
  let mut boot_failure_msg = None;
  while let Ok(ev) = worker.events_rx.try_recv() {
    if let WorkerEvents::BootFailure(failure) = &ev.event {
      boot_failure_msg = Some(failure.msg.clone());
    }
  }
  let msg = boot_failure_msg
    .expect("module-init failure must emit a BootFailure event");
  assert!(
    msg.contains("invalid source hash"),
    "the BootFailure event must carry the checksum error (got: {msg})"
  );
}

#[tokio::test]
#[serial]
async fn test_read_source_range_both_backings() {
  // One MiB of padding gives a deterministically named opaque module
  // (`pad://0`) to read ranges from.
  let bytes =
    generate_test_eszip_bytes("console.log('ranged reads work');", None, 1)
      .await;
  let dir = tempfile::tempdir().unwrap();
  let bundle_path = dir.path().join("bundle.eszip");
  std::fs::write(&bundle_path, &bytes).unwrap();

  let file_backed =
    payload_to_eszip(EszipPayloadKind::FileKind(bundle_path.clone()))
      .await
      .unwrap();
  let memory_backed = payload_to_eszip(EszipPayloadKind::VecKind(bytes))
    .await
    .unwrap();

  assert!(file_backed.is_file_backed());
  assert!(!memory_backed.is_file_backed());

  for (name, eszip) in [("file", &file_backed), ("memory", &memory_backed)] {
    let specifier = "pad://0";
    let full = eszip
      .read_source(specifier)
      .await
      .unwrap()
      .unwrap_or_else(|| panic!("source must exist ({name})"));
    assert_eq!(full.len(), 1 << 20);

    // `pos > 0` is the historically broken case (`VirtualFile::read_file`
    // used to ignore it).
    let ranged = eszip.read_source_range(specifier, 5, 7).await.unwrap();
    assert_eq!(
      ranged.as_slice(),
      &full[5..12],
      "ranged read must honor pos ({name})"
    );

    let at_eof = eszip
      .read_source_range(specifier, full.len() as u64 + 10, 4)
      .await
      .unwrap();
    assert!(at_eof.is_empty(), "reads past EOF yield nothing ({name})");
  }
}

// ---------------------------------------------------------------------------
// Memory + cold start
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[tokio::test]
#[serial]
async fn test_file_backed_rss_stays_within_budget() {
  // Padded to ~50 MiB: with file-backed loading only the header, metadata,
  // and the touched module sources should ever become resident.
  const PAD_MIB: usize = 50;
  const RSS_BUDGET_KIB: u64 = 32 * 1024;

  // Warm-up boot: absorbs the one-time process overhead (V8 snapshot, IO
  // runtime threads, TLS init, ...) so the measured delta isolates what the
  // 50 MiB bundle itself adds.
  {
    let bytes =
      generate_test_eszip_bytes("console.log('warm-up booted');", None, 0)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let warmup_path = dir.path().join("warmup.eszip");
    std::fs::write(&warmup_path, &bytes).unwrap();
    let mut worker =
      EszipWorker::try_boot(EszipPayloadKind::FileKind(warmup_path))
        .await
        .unwrap();
    worker.expect_log_containing("warm-up booted").await;
    worker.terminate().await;
  }

  let bytes = generate_test_eszip_bytes(
    "console.log('rss probe booted');",
    None,
    PAD_MIB,
  )
  .await;
  assert!(bytes.len() >= PAD_MIB << 20);

  let dir = tempfile::tempdir().unwrap();
  let bundle_path = dir.path().join("large.eszip");
  std::fs::write(&bundle_path, &bytes).unwrap();
  drop(bytes);

  let rss_before = vm_rss_kib();
  let mut worker =
    EszipWorker::try_boot(EszipPayloadKind::FileKind(bundle_path))
      .await
      .unwrap();
  worker.expect_log_containing("rss probe booted").await;
  let rss_delta = vm_rss_kib().saturating_sub(rss_before);
  worker.terminate().await;

  println!(
    "file-backed boot of a {PAD_MIB} MiB bundle: VmRSS delta = {} KiB \
     (budget {} KiB)",
    rss_delta, RSS_BUDGET_KIB
  );

  // Log-only comparison: the same bundle held fully in memory (the unchanged
  // servicePath-style `Eszip` payload).
  {
    // Baseline taken before generation: the `Eszip` payload holds its pad
    // blobs resident from the moment they're created.
    let rss_before = vm_rss_kib();
    let in_memory_eszip =
      generate_test_eszip("console.log('rss probe booted');", None, PAD_MIB)
        .await;
    let mut worker =
      EszipWorker::try_boot(EszipPayloadKind::Eszip(in_memory_eszip))
        .await
        .unwrap();
    worker.expect_log_containing("rss probe booted").await;
    let in_memory_delta = vm_rss_kib().saturating_sub(rss_before);
    worker.terminate().await;
    println!(
      "in-memory boot of the same bundle: VmRSS delta = {in_memory_delta} KiB"
    );
  }

  assert!(
    rss_delta < RSS_BUDGET_KIB,
    "file-backed boot must not make the bundle resident: VmRSS grew by \
     {rss_delta} KiB (budget {RSS_BUDGET_KIB} KiB)"
  );
}

#[tokio::test]
#[serial]
async fn test_file_backed_cold_start_guard() {
  const PAD_MIB: usize = 50;
  const CODE: &str = "console.log('cold start probe booted');";

  // In-memory baseline: the `Eszip` payload kind (the unchanged servicePath
  // branch) boots from the fully resident archive.
  let in_memory_eszip = generate_test_eszip(CODE, None, PAD_MIB).await;
  let in_memory_started = Instant::now();
  {
    let mut worker =
      EszipWorker::try_boot(EszipPayloadKind::Eszip(in_memory_eszip))
        .await
        .unwrap();
    worker
      .expect_log_containing("cold start probe booted")
      .await;
    worker.terminate().await;
  }
  let in_memory = in_memory_started.elapsed();

  let bytes = generate_test_eszip_bytes(CODE, None, PAD_MIB).await;
  let dir = tempfile::tempdir().unwrap();
  let bundle_path = dir.path().join("large.eszip");
  std::fs::write(&bundle_path, &bytes).unwrap();
  drop(bytes);

  let file_backed_started = Instant::now();
  {
    let mut worker =
      EszipWorker::try_boot(EszipPayloadKind::FileKind(bundle_path))
        .await
        .unwrap();
    worker
      .expect_log_containing("cold start probe booted")
      .await;
    worker.terminate().await;
  }
  let file_backed = file_backed_started.elapsed();

  println!(
    "cold start guard: file-backed boot = {file_backed:?}, in-memory boot = \
     {in_memory:?}"
  );
  assert!(
    file_backed <= in_memory * 2 + Duration::from_millis(500),
    "file-backed cold start regressed: {file_backed:?} vs {in_memory:?} \
     in-memory boot"
  );
}
