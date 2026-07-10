//! S3 fs-mount integration tests.
//!
//! Each test boots a user worker whose fixture (`tests/fixture/s3fs_ops`)
//! performs one filesystem operation against the `/s3` mount, driven by
//! `FS_TEST_*` env vars, and reports the outcome through console logs. The
//! logs come back over the worker events channel. Requires S3 credentials in
//! `tests/.env` (`S3FS_TEST_*`), otherwise the tests are ignored.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use base::utils::test_utils::test_user_runtime_opts;
use base::worker::TerminationToken;
use base::worker::WorkerSurfaceBuilder;
use ctor::ctor;
use deno_core::serde_json;
use deno_core::serde_json::json;
use ext_event_worker::events::LogLevel;
use ext_event_worker::events::WorkerEventWithMetadata;
use ext_event_worker::events::WorkerEvents;
use fs::s3_fs::S3FsConfig;
use once_cell::sync::Lazy;
use rand::Rng;
use rand::distributions::Alphanumeric;
use serde::Deserialize;
use serial_test::serial;
use tokio::sync::mpsc;
use tokio::time::timeout;
use uuid::Uuid;

const MIB: usize = 1024 * 1024;
const OP_DEADLINE: Duration = Duration::from_secs(300);

#[ctor]
fn init() {
  let _ = dotenvy::from_filename("./tests/.env");
}

fn is_supabase_storage_being_tested() -> bool {
  std::env::var("S3FS_TEST_SUPABASE_STORAGE").unwrap_or_default() == "true"
}

fn get_root_path() -> &'static str {
  static VALUE: Lazy<String> = Lazy::new(|| {
    rand::thread_rng()
      .sample_iter(&Alphanumeric)
      .take(10)
      .map(char::from)
      .collect()
  });

  VALUE.as_str()
}

fn get_path<P>(path: P) -> String
where
  P: AsRef<Path>,
{
  let path = path.as_ref().to_str().unwrap();

  if path.is_empty() {
    return get_root_path().to_string();
  }

  format!(
    "{}/{}",
    get_root_path(),
    path.strip_prefix('/').unwrap_or(path)
  )
}

fn s3_fs_config_from_env() -> S3FsConfig {
  let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
  let endpoint_url = env("S3FS_TEST_ENDPOINT_URL");

  serde_json::from_value(json!({
    "appName": env("S3FS_TEST_APP_NAME").unwrap_or("meowmeow".into()),
    "bucketName": env("S3FS_TEST_BUCKET_NAME")
      .expect("no bucket name were found for s3fs test"),
    "endpointUrl": endpoint_url,
    "forcePathStyle": endpoint_url.is_some(),
    "region": env("S3FS_TEST_REGION"),
    "credentials": {
      "accessKeyId": env("S3FS_TEST_ACCESS_KEY_ID")
        .expect("no credentials were found for s3fs test"),
      "secretAccessKey": env("S3FS_TEST_SECRET_ACCESS_KEY")
        .expect("no credentials were found for s3fs test"),
    },
    "retryConfig": {
      "mode": "standard",
    },
  }))
  .unwrap()
}

/// Boot the `s3fs_ops` fixture with the given op parameters and return the
/// info-level log lines the worker emitted (waits until an `op ...` marker or
/// worker shutdown).
async fn run_s3_op(env_vars: &[(&str, String)]) -> Vec<String> {
  let (events_tx, mut events_rx) =
    mpsc::unbounded_channel::<WorkerEventWithMetadata>();
  let termination_token = TerminationToken::new();

  let mut conf = test_user_runtime_opts();
  conf.key = Some(Uuid::new_v4());
  conf.events_msg_tx = Some(events_tx);
  conf.context = json!({ "useReadSyncFileAPI": true }).as_object().cloned();

  let surface = WorkerSurfaceBuilder::new()
    .init_opts(ext_workers::context::WorkerContextInitOpts {
      service_path: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixture/s3fs_ops"),
      no_module_cache: false,
      no_npm: None,
      env_vars: env_vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect::<HashMap<_, _>>(),
      timing: None,
      maybe_eszip: None,
      maybe_entrypoint: None,
      maybe_module_code: None,
      conf: Box::new(conf),
      static_patterns: vec![],
      maybe_s3_fs_config: Some(s3_fs_config_from_env().into()),
      maybe_tmp_fs_config: None,
      maybe_http_fs_config: None,
      maybe_otel_config: None,
    })
    .termination_token(termination_token.clone())
    .eager_module_init(true)
    .build()
    .await
    .unwrap();

  let mut logs = Vec::new();
  let collect_fut = async {
    while let Some(ev) = events_rx.recv().await {
      if let WorkerEvents::Log(log) = ev.event {
        if log.level != LogLevel::Info {
          continue;
        }
        let is_marker = log.msg.starts_with("op ");
        logs.push(log.msg);
        if is_marker {
          break;
        }
      }
    }
  };

  if timeout(OP_DEADLINE, collect_fut).await.is_err() {
    panic!("timed out waiting for the fixture's op marker; logs: {logs:?}");
  }

  let _ =
    timeout(Duration::from_secs(30), termination_token.cancel_and_wait()).await;
  drop(surface);

  logs
}

async fn expect_op_ok(env_vars: &[(&str, String)], marker: &str) -> String {
  let logs = run_s3_op(env_vars).await;
  let last = logs.last().cloned().unwrap_or_default();
  assert!(
    last.starts_with(marker),
    "expected marker {marker:?}, got logs: {logs:?}"
  );
  last
}

fn op_env(
  op: &str,
  path: &str,
  size: Option<usize>,
  recursive: Option<bool>,
) -> Vec<(&'static str, String)> {
  let mut vars = vec![
    ("FS_TEST_OP", op.to_string()),
    ("FS_TEST_PATH", get_path(path)),
  ];
  if let Some(size) = size {
    vars.push(("FS_TEST_SIZE", size.to_string()));
  }
  if let Some(recursive) = recursive {
    vars.push(("FS_TEST_RECURSIVE", recursive.to_string()));
  }
  vars
}

async fn remove(path: &str, recursive: bool) {
  expect_op_ok(&op_env("remove", path, None, Some(recursive)), "op ").await;
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DenoDirEntry {
  name: String,
  is_file: bool,
  is_directory: bool,
}

async fn read_dir(path: &str) -> HashMap<String, DenoDirEntry> {
  let marker =
    expect_op_ok(&op_env("read-dir", path, None, None), "op read-dir ok ")
      .await;
  let payload = marker.strip_prefix("op read-dir ok ").unwrap();
  serde_json::from_str::<Vec<DenoDirEntry>>(payload)
    .unwrap()
    .into_iter()
    .map(|it| (it.name.clone(), it))
    .collect()
}

async fn test_write_and_verify_bytes(bytes: usize) {
  remove("", true).await;

  expect_op_ok(
    &op_env("write", "meow.bin", Some(bytes), None),
    "op write ok",
  )
  .await;
  expect_op_ok(
    &op_env("verify", "meow.bin", Some(bytes), None),
    "op verify ok",
  )
  .await;
}

#[cfg_attr(not(dotenv), ignore)]
#[serial]
#[tokio::test]
async fn test_write_and_get_various_bytes() {
  test_write_and_verify_bytes(0).await;
  test_write_and_verify_bytes(1).await;
  test_write_and_verify_bytes(3 * MIB).await;
  test_write_and_verify_bytes(5 * MIB).await;
  test_write_and_verify_bytes(8 * MIB).await;
  test_write_and_verify_bytes(50 * MIB).await;
}

/// This test is to ensure that the Upload file size limit in the storage
/// settings section is working properly.
///
/// Note that the test below assumes an upload file size limit of 50 MiB.
/// This limit is specific to Supabase Storage and does not apply to MinIO.
///
/// See: https://supabase.com/docs/guides/storage/uploads/file-limits
#[cfg_attr(not(dotenv), ignore)]
#[tokio::test]
#[serial]
async fn test_write_and_get_over_50_mib() {
  if !is_supabase_storage_being_tested() {
    return;
  }
  remove("", true).await;

  expect_op_ok(&op_env("write", "meow.bin", Some(51 * MIB), None), "op ").await;

  let logs =
    run_s3_op(&op_env("verify", "meow.bin", Some(51 * MIB), None)).await;
  let last = logs.last().cloned().unwrap_or_default();
  assert!(
    last.starts_with("op failed:") && last.contains("NotFound"),
    "expected a NotFound failure, got: {logs:?}"
  );
}

#[cfg_attr(not(dotenv), ignore)]
#[tokio::test]
#[serial]
async fn test_mkdir_and_read_dir() {
  remove("", true).await;

  expect_op_ok(&op_env("mkdir", "a", None, Some(true)), "op mkdir ok").await;

  let value = read_dir("").await;
  assert!(value.contains_key("a"));
  assert!(value.get("a").unwrap().is_directory);
}

#[cfg_attr(not(dotenv), ignore)]
#[tokio::test]
#[serial]
async fn test_mkdir_recursive_and_read_dir() {
  remove("", true).await;

  expect_op_ok(
    &op_env("mkdir", "a/b/c/meow", None, Some(true)),
    "op mkdir ok",
  )
  .await;

  for [dir, expected] in
    [["", "a"], ["a", "b"], ["a/b", "c"], ["a/b/c", "meow"]]
  {
    let value = read_dir(dir).await;
    assert!(value.contains_key(expected));
    assert!(value.get(expected).unwrap().is_directory);
  }
}

#[cfg_attr(not(dotenv), ignore)]
#[tokio::test]
#[serial]
async fn test_mkdir_with_no_recursive_opt_must_check_parent_path_exists() {
  remove("", true).await;

  expect_op_ok(&op_env("mkdir", "a", None, Some(true)), "op mkdir ok").await;

  let logs = run_s3_op(&op_env("mkdir", "a/b/c", None, Some(false))).await;
  let last = logs.last().cloned().unwrap_or_default();
  assert!(
    last.starts_with("op failed:")
      && last.contains("No such file or directory"),
    "expected a missing-parent failure, got: {logs:?}"
  );
}

#[cfg_attr(not(dotenv), ignore)]
#[tokio::test]
#[serial]
async fn test_mkdir_recursive_and_remove_recursive() {
  remove("", true).await;

  expect_op_ok(
    &op_env("mkdir", "a/b/c/meow", None, Some(true)),
    "op mkdir ok",
  )
  .await;
  expect_op_ok(
    &op_env("write", "a/b/c/meeeeow.bin", Some(11 * MIB), None),
    "op write ok",
  )
  .await;

  {
    let value = read_dir("a/b/c").await;

    assert_eq!(
      value.len(),
      if is_supabase_storage_being_tested() {
        // .emptyFolderPlaceholder in Supabase Storage
        3
      } else {
        2
      }
    );
    assert!(value.contains_key("meow"));
    assert!(value.get("meow").unwrap().is_directory);
    assert!(value.contains_key("meeeeow.bin"));
    assert!(value.get("meeeeow.bin").unwrap().is_file);
  }

  remove("a/b/c", true).await;
  remove("a/b", true).await;

  {
    let value = read_dir("a").await;
    assert_eq!(
      value.len(),
      if is_supabase_storage_being_tested() {
        // .emptyFolderPlaceholder in Supabase Storage
        1
      } else {
        0
      }
    );
  }

  {
    let value = read_dir("").await;
    assert_eq!(
      value.len(),
      if is_supabase_storage_being_tested() {
        // .emptyFolderPlaceholder in Supabase Storage
        2
      } else {
        1
      }
    );
    assert!(value.contains_key("a"));
    assert!(value.get("a").unwrap().is_directory);
  }

  remove("a", true).await;

  {
    let value = read_dir("").await;
    assert_eq!(
      value.len(),
      if is_supabase_storage_being_tested() {
        // .emptyFolderPlaceholder in Supabase Storage
        1
      } else {
        0
      }
    );
  }
}

#[cfg_attr(not(dotenv), ignore)]
#[tokio::test]
#[serial]
async fn test_ensure_using_sync_api_in_async_callback_is_not_allowed() {
  remove("", true).await;

  expect_op_ok(
    &op_env("write", "meow.bin", Some(1024), None),
    "op write ok",
  )
  .await;
  expect_op_ok(
    &op_env("read-sync-in-async", "meow.bin", None, None),
    "op read-sync-in-async blocked",
  )
  .await;
}
