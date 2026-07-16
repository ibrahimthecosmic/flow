//! File-backed eszip bundle cache + shared-bundle registry.
//!
//! Every `maybeEszip` input to `userWorkers.create` converges here: byte
//! buffers and streams are spilled into a content-addressed cache directory
//! (`<xxh3-64>.eszip`, atomic tmp+rename), and path inputs are used in place.
//! [`open_shared`] then parses the archive header once per canonical path and
//! hands out a shared, immutable [`SharedBundle`] whose data section serves
//! module sources with positional reads — the bundle bytes never become
//! resident; the OS page cache is the only cache.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;

use anyhow::Context;
use deno::deno_npm::resolution::ValidSerializedNpmResolutionSnapshot;
use eszip::v2::EszipV2Modules;
use futures::AsyncSeekExt;
use futures::io::AllowStdIo;
use futures::io::BufReader;
use serde::Deserialize;
use serde::Serialize;
use xxhash_rust::xxh3::Xxh3;
use xxhash_rust::xxh3::xxh3_64;

use super::EszipDataSection;
use super::parse;

const DEFAULT_BUNDLE_TTL_SECS: u64 = 7 * 24 * 60 * 60;
const TMP_TTL_SECS: u64 = 60 * 60;

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Where spilled bundles land: `$FLOW_BUNDLE_CACHE_DIR`, falling back to
/// `<tmpdir>/flow-bundles`.
pub fn cache_dir() -> PathBuf {
  std::env::var_os("FLOW_BUNDLE_CACHE_DIR")
    .map(PathBuf::from)
    .unwrap_or_else(|| std::env::temp_dir().join("flow-bundles"))
}

fn bundle_ttl() -> Duration {
  Duration::from_secs(
    std::env::var("FLOW_BUNDLE_CACHE_TTL_SECS")
      .ok()
      .and_then(|it| it.parse().ok())
      .unwrap_or(DEFAULT_BUNDLE_TTL_SECS),
  )
}

fn tmp_path(dir: &Path, tag: &str) -> PathBuf {
  dir.join(format!(
    ".{tag}.{}.{}.tmp",
    std::process::id(),
    TMP_SEQ.fetch_add(1, Ordering::Relaxed)
  ))
}

/// Unlinks expired cache entries: `*.eszip` older than the bundle TTL and
/// `*.tmp` leftovers older than an hour. Runs at most once per process.
pub fn sweep_once() {
  static SWEPT: OnceLock<()> = OnceLock::new();
  SWEPT.get_or_init(|| {
    let Ok(entries) = std::fs::read_dir(cache_dir()) else {
      return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
      let path = entry.path();
      let ttl = match path.extension().and_then(|it| it.to_str()) {
        // "json" covers the `<hash>.url.json` URL-download manifests.
        Some("eszip") | Some("json") => bundle_ttl(),
        Some("tmp") => Duration::from_secs(TMP_TTL_SECS),
        _ => continue,
      };
      let expired = entry
        .metadata()
        .and_then(|it| it.modified())
        .ok()
        .and_then(|mtime| now.duration_since(mtime).ok())
        .is_some_and(|age| age > ttl);
      if expired {
        let _ = std::fs::remove_file(&path);
      }
    }
  });
}

/// Refreshes `dest`'s mtime so the TTL sweep sees it as recently used.
/// Returns false when the file doesn't exist.
fn touch(dest: &Path) -> bool {
  match std::fs::OpenOptions::new().write(true).open(dest) {
    Ok(file) => {
      let _ = file.set_modified(SystemTime::now());
      true
    }
    Err(_) => false,
  }
}

/// Stores `bytes` in the content-addressed bundle cache and returns the
/// resulting `<xxh3>.eszip` path. Identical inputs converge on one file.
pub async fn store_bytes(
  bytes: impl AsRef<[u8]> + Send + 'static,
) -> Result<PathBuf, anyhow::Error> {
  fs::IO_RT
    .spawn_blocking(move || store_bytes_blocking(bytes.as_ref()))
    .await
    .context("the bundle cache write task failed")?
}

fn store_bytes_blocking(bytes: &[u8]) -> Result<PathBuf, anyhow::Error> {
  sweep_once();
  let dir = cache_dir();
  std::fs::create_dir_all(&dir).with_context(|| {
    format!("failed to create the bundle cache dir at {}", dir.display())
  })?;

  let hash = xxh3_64(bytes);
  let dest = dir.join(format!("{hash:016x}.eszip"));
  if touch(&dest) {
    return Ok(dest);
  }

  let tmp = tmp_path(&dir, &format!("{hash:016x}"));
  std::fs::write(&tmp, bytes)
    .and_then(|_| std::fs::rename(&tmp, &dest))
    .inspect_err(|_| {
      let _ = std::fs::remove_file(&tmp);
    })
    .with_context(|| {
      format!("failed to write the bundle to {}", dest.display())
    })?;

  Ok(dest)
}

/// A recorded eszip download: which content-addressed blob a URL's bundle
/// landed in, plus what is needed to decide whether the network can be
/// skipped next time — an explicit `version` pin, or the HTTP validators
/// (`ETag`/`Last-Modified`) for a conditional re-fetch.
///
/// Serialized camelCase because entries travel through the
/// `op_eszip_url_cache_*` ops straight to/from JS.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UrlCacheEntry {
  pub url: String,
  #[serde(default)]
  pub version: Option<String>,
  #[serde(default)]
  pub etag: Option<String>,
  #[serde(default)]
  pub last_modified: Option<String>,
  pub bundle_path: PathBuf,
}

/// Meta file for a `(url, version)` download: `<xxh3-64>.url.json` in the
/// cache dir. The stored entry echoes url/version so a hash collision reads
/// as a miss instead of serving the wrong bundle.
fn url_meta_path(url: &str, version: Option<&str>) -> PathBuf {
  let mut hasher = Xxh3::default();
  hasher.update(url.as_bytes());
  hasher.update(b"\0");
  hasher.update(version.unwrap_or("").as_bytes());
  cache_dir().join(format!("{:016x}.url.json", hasher.digest()))
}

/// Looks up the recorded download for `(url, version)`. A hit touches both
/// the meta file and the referenced bundle so the TTL sweep sees them as
/// used; a meta file whose bundle was swept (or that fails to parse) is
/// removed and reads as a miss.
pub async fn url_cache_lookup(
  url: String,
  version: Option<String>,
) -> Result<Option<UrlCacheEntry>, anyhow::Error> {
  fs::IO_RT
    .spawn_blocking(move || url_cache_lookup_blocking(&url, version.as_deref()))
    .await
    .context("the bundle cache lookup task failed")?
}

fn url_cache_lookup_blocking(
  url: &str,
  version: Option<&str>,
) -> Result<Option<UrlCacheEntry>, anyhow::Error> {
  sweep_once();
  let meta_path = url_meta_path(url, version);
  let bytes = match std::fs::read(&meta_path) {
    Ok(bytes) => bytes,
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(e) => {
      return Err(e).with_context(|| {
        format!("failed to read the url manifest at {}", meta_path.display())
      });
    }
  };

  let Ok(entry) = serde_json::from_slice::<UrlCacheEntry>(&bytes) else {
    let _ = std::fs::remove_file(&meta_path);
    return Ok(None);
  };
  if entry.url != url || entry.version.as_deref() != version {
    return Ok(None);
  }
  if !touch(&entry.bundle_path) {
    let _ = std::fs::remove_file(&meta_path);
    return Ok(None);
  }
  let _ = touch(&meta_path);
  Ok(Some(entry))
}

/// Records a finished download in the url manifest (atomic tmp+rename;
/// replaces any previous entry for the same `(url, version)`).
pub async fn url_cache_record(
  entry: UrlCacheEntry,
) -> Result<(), anyhow::Error> {
  fs::IO_RT
    .spawn_blocking(move || url_cache_record_blocking(&entry))
    .await
    .context("the bundle cache record task failed")?
}

fn url_cache_record_blocking(
  entry: &UrlCacheEntry,
) -> Result<(), anyhow::Error> {
  let dir = cache_dir();
  std::fs::create_dir_all(&dir).with_context(|| {
    format!("failed to create the bundle cache dir at {}", dir.display())
  })?;
  let dest = url_meta_path(&entry.url, entry.version.as_deref());
  let tmp = tmp_path(&dir, "urlmeta");
  let json = serde_json::to_vec(entry)
    .context("failed to serialize the url manifest entry")?;
  std::fs::write(&tmp, json)
    .and_then(|_| std::fs::rename(&tmp, &dest))
    .inspect_err(|_| {
      let _ = std::fs::remove_file(&tmp);
    })
    .with_context(|| {
      format!("failed to write the url manifest at {}", dest.display())
    })?;
  Ok(())
}

/// Incremental spill of a streamed bundle: chunks are hashed (xxh3) while
/// being written to a temp file; [`Self::finish`] renames the file to its
/// content-addressed name. Dropping an unfinished spill unlinks the temp file.
pub struct SpillFile {
  /// `None` after finish, or transiently while a blocking write is in flight
  /// (a cancelled write leaves the temp file to the TTL sweep).
  state: Option<(std::fs::File, Box<Xxh3>)>,
  tmp_path: PathBuf,
}

impl SpillFile {
  pub async fn create() -> Result<Self, anyhow::Error> {
    fs::IO_RT
      .spawn_blocking(|| {
        sweep_once();
        let dir = cache_dir();
        std::fs::create_dir_all(&dir).with_context(|| {
          format!("failed to create the bundle cache dir at {}", dir.display())
        })?;
        let tmp_path = tmp_path(&dir, "spill");
        let file = std::fs::File::create(&tmp_path).with_context(|| {
          format!("failed to create the spill file at {}", tmp_path.display())
        })?;
        Ok(Self {
          state: Some((file, Box::default())),
          tmp_path,
        })
      })
      .await
      .context("the bundle spill task failed")?
  }

  pub async fn write(&mut self, chunk: Vec<u8>) -> Result<(), anyhow::Error> {
    let (mut file, mut hasher) = self
      .state
      .take()
      .context("the spill file is already closed")?;
    let (file, hasher, result) = fs::IO_RT
      .spawn_blocking(move || {
        hasher.update(&chunk);
        let result = file.write_all(&chunk);
        (file, hasher, result)
      })
      .await
      .context("the bundle spill task failed")?;
    self.state = Some((file, hasher));
    result.context("failed to write to the spill file")
  }

  /// Finalizes the spill: syncs the temp file and renames it to its
  /// content-addressed `<xxh3>.eszip` path (reusing an existing entry with
  /// identical content).
  pub async fn finish(mut self) -> Result<PathBuf, anyhow::Error> {
    let (file, hasher) = self
      .state
      .take()
      .context("the spill file is already closed")?;
    let tmp = self.tmp_path.clone();
    fs::IO_RT
      .spawn_blocking(move || {
        let cleanup = scopeguard::guard((), |()| {
          let _ = std::fs::remove_file(&tmp);
        });
        file.sync_all().context("failed to sync the spill file")?;
        drop(file);

        let dest = cache_dir().join(format!("{:016x}.eszip", hasher.digest()));
        if !touch(&dest) {
          std::fs::rename(&tmp, &dest).with_context(|| {
            format!("failed to store the bundle at {}", dest.display())
          })?;
        }
        // `touch` hit: an identical bundle is already cached; the guard
        // removes our duplicate temp file. Rename success: the temp path is
        // gone and the removal is a harmless no-op.
        drop(cleanup);
        Ok(dest)
      })
      .await
      .context("the bundle spill task failed")?
  }
}

impl Drop for SpillFile {
  fn drop(&mut self) {
    if self.state.take().is_some() {
      let _ = std::fs::remove_file(&self.tmp_path);
    }
  }
}

/// A parsed, file-backed bundle shared by every worker created from the same
/// canonical path: the immutable header modules, the pristine npm snapshot
/// (cloned into each worker's view), and the pread-serving data section.
#[derive(Debug)]
pub struct SharedBundle {
  pub modules: EszipV2Modules,
  pub options: eszip::v2::Options,
  pub npm_snapshot: Option<ValidSerializedNpmResolutionSnapshot>,
  pub data_section: Arc<EszipDataSection>,
}

/// Identity of the file contents a [`SharedBundle`] was parsed from; a
/// mismatch (bundle replaced on disk) forces a reparse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
  len: u64,
  mtime: Option<SystemTime>,
  #[cfg(unix)]
  dev: u64,
  #[cfg(unix)]
  ino: u64,
}

impl FileStamp {
  fn for_path(path: &Path) -> std::io::Result<Self> {
    let metadata = std::fs::metadata(path)?;
    Ok(Self {
      len: metadata.len(),
      mtime: metadata.modified().ok(),
      #[cfg(unix)]
      dev: std::os::unix::fs::MetadataExt::dev(&metadata),
      #[cfg(unix)]
      ino: std::os::unix::fs::MetadataExt::ino(&metadata),
    })
  }
}

/// Per-path registry cell. The cell-level mutex also serializes parsing, so
/// concurrent creates of the same path share a single parse.
#[derive(Default)]
struct CacheCell {
  cached: Option<(FileStamp, Weak<SharedBundle>)>,
}

fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<CacheCell>>>> {
  static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<CacheCell>>>>> =
    OnceLock::new();
  REGISTRY.get_or_init(Default::default)
}

/// Opens (or reuses) the shared parsed view of the bundle at `path`,
/// deduplicating by canonicalized path. The registry holds only weak
/// references: a bundle is dropped once its last worker goes away.
pub async fn open_shared(
  path: &Path,
) -> Result<Arc<SharedBundle>, anyhow::Error> {
  let path = path.to_path_buf();
  fs::IO_RT
    .spawn_blocking(move || open_shared_blocking(&path))
    .await
    .context("the bundle open task failed")?
}

fn open_shared_blocking(
  path: &Path,
) -> Result<Arc<SharedBundle>, anyhow::Error> {
  sweep_once();
  let canonical = std::fs::canonicalize(path).with_context(|| {
    format!("failed to open the eszip bundle at {}", path.display())
  })?;
  let stamp = FileStamp::for_path(&canonical)?;

  let cell = {
    let mut registry = registry().lock().unwrap();
    // Opportunistically drop cells whose bundle died. A locked cell has an
    // open in flight — leave it alone.
    registry.retain(|_, cell| {
      Arc::strong_count(cell) > 1
        || cell.try_lock().is_ok_and(|cell| {
          cell
            .cached
            .as_ref()
            .is_some_and(|(_, weak)| weak.strong_count() > 0)
        })
    });
    registry.entry(canonical.clone()).or_default().clone()
  };

  let mut cell = cell.lock().unwrap();
  if let Some((cached_stamp, weak)) = &cell.cached
    && *cached_stamp == stamp
    && let Some(shared) = weak.upgrade()
  {
    return Ok(shared);
  }

  let shared = Arc::new(parse_bundle(&canonical).with_context(|| {
    format!(
      "failed to parse the eszip bundle at {}",
      canonical.display()
    )
  })?);
  cell.cached = Some((stamp, Arc::downgrade(&shared)));
  Ok(shared)
}

/// Parses the archive header from disk and wires the file handle into a
/// pread-serving [`EszipDataSection`]. The data section (not the header
/// bytes) is what stays alive for the bundle's lifetime.
fn parse_bundle(path: &Path) -> Result<SharedBundle, anyhow::Error> {
  let file = std::fs::File::open(path)?;
  let (eszip, initial_offset, file) =
    futures::executor::block_on(async move {
      let mut io = AllowStdIo::new(file);
      let mut bufreader = BufReader::new(&mut io);
      let eszip = parse::parse_v2_header(&mut bufreader).await?;
      let initial_offset = bufreader.stream_position().await?;
      drop(bufreader);
      Ok::<_, anyhow::Error>((eszip, initial_offset, io.into_inner()))
    })?;

  let data_section = EszipDataSection::new_file(
    file,
    initial_offset,
    eszip.modules.clone(),
    eszip.options,
  )?;

  Ok(SharedBundle {
    modules: eszip.modules,
    options: eszip.options,
    npm_snapshot: eszip.npm_snapshot,
    data_section: Arc::new(data_section),
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  /// One test fn (not several) because `cache_dir()` reads a process-global
  /// env var and cargo runs test fns concurrently.
  #[test]
  fn url_manifest_roundtrip_and_invalidation() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: single-threaded at this point in the test binary aside from
    // other tests, none of which read FLOW_BUNDLE_CACHE_DIR.
    unsafe {
      std::env::set_var("FLOW_BUNDLE_CACHE_DIR", dir.path());
    }

    let bundle_path = dir.path().join("cafebabe.eszip");
    std::fs::write(&bundle_path, b"not a real bundle").unwrap();

    let entry = UrlCacheEntry {
      url: "https://example.com/app.eszip".into(),
      version: None,
      etag: Some("\"abc\"".into()),
      last_modified: None,
      bundle_path: bundle_path.clone(),
    };

    // Miss before anything is recorded.
    assert!(
      url_cache_lookup_blocking(&entry.url, None)
        .unwrap()
        .is_none()
    );

    // Record → hit, with the validators intact.
    url_cache_record_blocking(&entry).unwrap();
    let hit = url_cache_lookup_blocking(&entry.url, None)
      .unwrap()
      .unwrap();
    assert_eq!(hit.bundle_path, bundle_path);
    assert_eq!(hit.etag.as_deref(), Some("\"abc\""));

    // A versioned entry for the same url is a distinct key.
    assert!(
      url_cache_lookup_blocking(&entry.url, Some("1.0.0"))
        .unwrap()
        .is_none()
    );
    let versioned = UrlCacheEntry {
      version: Some("1.0.0".into()),
      etag: None,
      ..entry.clone()
    };
    url_cache_record_blocking(&versioned).unwrap();
    assert!(
      url_cache_lookup_blocking(&entry.url, Some("1.0.0"))
        .unwrap()
        .is_some()
    );
    // ... and re-recording it replaces rather than duplicates.
    url_cache_record_blocking(&versioned).unwrap();

    // The unversioned entry is still there and independent.
    assert!(
      url_cache_lookup_blocking(&entry.url, None)
        .unwrap()
        .is_some()
    );

    // A swept-away bundle turns the entry into a miss and drops the meta.
    std::fs::remove_file(&bundle_path).unwrap();
    assert!(
      url_cache_lookup_blocking(&entry.url, None)
        .unwrap()
        .is_none()
    );
    assert!(!url_meta_path(&entry.url, None).exists());

    // Corrupt meta reads as a miss and is cleaned up.
    let meta = url_meta_path(&entry.url, Some("2.0.0"));
    std::fs::write(&meta, b"{ not json").unwrap();
    assert!(
      url_cache_lookup_blocking(&entry.url, Some("2.0.0"))
        .unwrap()
        .is_none()
    );
    assert!(!meta.exists());
  }
}
