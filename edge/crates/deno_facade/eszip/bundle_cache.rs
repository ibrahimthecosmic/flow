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

/// Soft size cap in bytes (`$FLOW_BUNDLE_CACHE_MAX_SIZE`, plain integer).
/// Unset or unparseable means uncapped — the pre-cap behavior.
fn max_cache_size() -> Option<u64> {
  std::env::var("FLOW_BUNDLE_CACHE_MAX_SIZE")
    .ok()?
    .parse()
    .ok()
}

fn tmp_path(dir: &Path, tag: &str) -> PathBuf {
  dir.join(format!(
    ".{tag}.{}.{}.tmp",
    std::process::id(),
    TMP_SEQ.fetch_add(1, Ordering::Relaxed)
  ))
}

#[derive(Debug, Clone, Copy)]
struct IndexEntry {
  size: u64,
  last_used: SystemTime,
}

/// In-memory view of the cache dir's `*.eszip` blobs, used for the size cap
/// and `stats()`. The disk stays the source of truth: every admission
/// rebuilds it from a readdir (see [`sweep_and_make_room`]), while cache
/// hits only bump `last_used` — the read paths never pay for a scan.
#[derive(Default)]
struct CacheIndex {
  built: bool,
  entries: HashMap<PathBuf, IndexEntry>,
  total_bytes: u64,
}

fn index() -> &'static Mutex<CacheIndex> {
  static INDEX: OnceLock<Mutex<CacheIndex>> = OnceLock::new();
  INDEX.get_or_init(Default::default)
}

/// What a [`CacheEvent`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheEventAction {
  /// A blob was unlinked (LRU pressure or an explicit evict).
  Evicted,
  /// An admission could not get under the cap and proceeded over it.
  OverCap,
  /// A TTL sweep removed expired entries.
  Sweep,
}

/// A bundle-cache activity notice for embedder observability. flow's cli
/// layer forwards these onto the `FlowRuntime.events` stream; this crate
/// only knows the type-erased sink (deno_facade must not depend on the
/// worker-pool event machinery).
#[derive(Debug, Clone)]
pub struct CacheEvent {
  pub action: CacheEventAction,
  /// The manifest key, when the action targeted one (explicit evict).
  pub cache_key: Option<String>,
  /// The blob involved, when the action targeted one.
  pub path: Option<PathBuf>,
  /// Bytes the action acted on (evicted/swept bytes; the incoming bundle
  /// size for `OverCap`).
  pub bytes: u64,
  /// Cache total after the action (0 until the accounting index is built).
  pub total_bytes: u64,
  /// The configured cap, when one is set.
  pub max_bytes: Option<u64>,
}

/// Registers the process-wide observer for cache activity (first caller
/// wins). flow wires this to the user-worker event stream at startup.
pub fn set_event_sink(sink: Box<dyn Fn(CacheEvent) + Send + Sync>) {
  let _ = event_sink().set(sink);
}

#[allow(
  clippy::type_complexity,
  reason = "a one-off boxed callback type; an alias would only add indirection"
)]
fn event_sink() -> &'static OnceLock<Box<dyn Fn(CacheEvent) + Send + Sync>> {
  static SINK: OnceLock<Box<dyn Fn(CacheEvent) + Send + Sync>> =
    OnceLock::new();
  &SINK
}

fn emit(event: CacheEvent) {
  if let Some(sink) = event_sink().get() {
    sink(event);
  }
}

/// Records a just-landed blob in the index (no-op until the first admission
/// or stats call builds it).
fn index_admit(path: &Path, size: u64) {
  let mut index = index().lock().unwrap();
  if !index.built {
    return;
  }
  let previous = index.entries.insert(
    path.to_path_buf(),
    IndexEntry {
      size,
      last_used: SystemTime::now(),
    },
  );
  index.total_bytes =
    index.total_bytes - previous.map(|it| it.size).unwrap_or(0) + size;
}

/// Forgets an unlinked blob.
fn index_remove(path: &Path) {
  let mut index = index().lock().unwrap();
  if let Some(entry) = index.entries.remove(path) {
    index.total_bytes -= entry.size;
  }
}

/// Whether the blob at `path` is backing (or about to back) a live worker:
/// its canonical path has a live [`SharedBundle`] in the registry, or an
/// open is in flight on its cell. Purely in-process bookkeeping — an
/// unlinked-anyway blob stays readable through open fds on Unix, so a stale
/// answer costs at worst a re-download, never a broken worker.
fn is_pinned(path: &Path) -> bool {
  let Ok(canonical) = std::fs::canonicalize(path) else {
    return false;
  };
  let Some(cell) = registry().lock().unwrap().get(&canonical).cloned() else {
    return false;
  };
  Arc::strong_count(&cell) > 1
    || match cell.try_lock() {
      // A locked cell has an open in flight.
      Err(_) => true,
      Ok(cell) => cell
        .cached
        .as_ref()
        .is_some_and(|(_, weak)| weak.strong_count() > 0),
    }
}

/// Admission-time maintenance, run before every spill/store/seed: rebuilds
/// the index from a readdir (self-healing external changes), TTL-expires
/// stale blobs, stale/orphaned `*.url.json` manifests and old `*.tmp`
/// leftovers, then — when `$FLOW_BUNDLE_CACHE_MAX_SIZE` is set — evicts
/// least-recently-used unpinned blobs until `incoming` more bytes fit under
/// the cap. The cap is soft: when eviction can't make room (everything left
/// is pinned, or the bundle alone exceeds the cap) the admission proceeds
/// over cap with a warning rather than failing a worker boot.
///
/// `protect` marks the blob being admitted right now (reconciling after a
/// spill whose size was unknown up front) so it is never its own victim.
fn sweep_and_make_room(incoming: u64, protect: Option<&Path>) {
  let dir = cache_dir();
  let now = SystemTime::now();
  let ttl = bundle_ttl();
  let tmp_ttl = Duration::from_secs(TMP_TTL_SECS);
  // Emitted after the index lock drops — the sink is foreign code.
  let mut events: Vec<CacheEvent> = Vec::new();

  let mut index = index().lock().unwrap();
  index.built = true;
  index.entries.clear();
  index.total_bytes = 0;

  let Ok(dir_entries) = std::fs::read_dir(&dir) else {
    return;
  };

  let mut manifests = Vec::new();
  let mut swept_bytes = 0u64;
  let mut swept_count = 0u64;
  for entry in dir_entries.flatten() {
    let path = entry.path();
    let Ok(metadata) = entry.metadata() else {
      continue;
    };
    let mtime = metadata.modified().unwrap_or(now);
    let age = now.duration_since(mtime).unwrap_or_default();
    match path.extension().and_then(|it| it.to_str()) {
      Some("eszip") => {
        if age > ttl
          && Some(path.as_path()) != protect
          && std::fs::remove_file(&path).is_ok()
        {
          swept_bytes += metadata.len();
          swept_count += 1;
          continue;
        }
        index.entries.insert(
          path,
          IndexEntry {
            size: metadata.len(),
            last_used: mtime,
          },
        );
        index.total_bytes += metadata.len();
      }
      Some("json") => manifests.push((path, age)),
      Some("tmp") if age > tmp_ttl => {
        let _ = std::fs::remove_file(&path);
      }
      _ => {}
    }
  }

  // A manifest goes when it expired, no longer parses, or points at a blob
  // that is gone (evicted/swept out from under it).
  for (path, age) in manifests {
    let dead = age > ttl
      || std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<UrlCacheEntry>(&bytes).ok())
        .is_none_or(|entry| !entry.bundle_path.exists());
    if dead {
      let _ = std::fs::remove_file(&path);
    }
  }

  if swept_count > 0 {
    log::info!(
      "flow bundle cache: TTL sweep removed {swept_count} bundle(s), {swept_bytes} bytes"
    );
    events.push(CacheEvent {
      action: CacheEventAction::Sweep,
      cache_key: None,
      path: None,
      bytes: swept_bytes,
      total_bytes: index.total_bytes,
      max_bytes: max_cache_size(),
    });
  }

  if let Some(cap) = max_cache_size()
    && index.total_bytes.saturating_add(incoming) > cap
  {
    let mut candidates: Vec<(PathBuf, IndexEntry)> = index
      .entries
      .iter()
      .filter(|(path, _)| Some(path.as_path()) != protect)
      .map(|(path, entry)| (path.clone(), *entry))
      .collect();
    candidates.sort_by_key(|(_, entry)| entry.last_used);

    for (path, entry) in candidates {
      if index.total_bytes.saturating_add(incoming) <= cap {
        break;
      }
      if is_pinned(&path) {
        continue;
      }
      match std::fs::remove_file(&path) {
        Ok(()) => {
          index.entries.remove(&path);
          index.total_bytes -= entry.size;
          log::info!(
            "flow bundle cache: evicted {} ({} bytes, LRU)",
            path.display(),
            entry.size
          );
          events.push(CacheEvent {
            action: CacheEventAction::Evicted,
            cache_key: None,
            path: Some(path),
            bytes: entry.size,
            total_bytes: index.total_bytes,
            max_bytes: Some(cap),
          });
        }
        // Windows refuses to unlink an open file; skip and retry on a
        // later admission.
        Err(e) => {
          log::debug!(
            "flow bundle cache: could not evict {}: {e}",
            path.display()
          );
        }
      }
    }

    if index.total_bytes.saturating_add(incoming) > cap {
      log::warn!(
        "flow bundle cache: over the {cap}-byte cap ({} bytes cached + {incoming} incoming) — everything else is in use; admitting anyway",
        index.total_bytes
      );
      events.push(CacheEvent {
        action: CacheEventAction::OverCap,
        cache_key: None,
        path: None,
        bytes: incoming,
        total_bytes: index.total_bytes,
        max_bytes: Some(cap),
      });
    }
  }

  drop(index);
  for event in events {
    emit(event);
  }
}

/// Refreshes `dest`'s mtime (the persistent LRU clock) and its in-memory
/// `last_used` so neither the TTL sweep nor the LRU eviction sees a hot
/// entry as stale. Returns false when the file doesn't exist.
fn touch(dest: &Path) -> bool {
  match std::fs::OpenOptions::new().write(true).open(dest) {
    Ok(file) => {
      let now = SystemTime::now();
      let _ = file.set_modified(now);
      let mut index = index().lock().unwrap();
      if let Some(entry) = index.entries.get_mut(dest) {
        entry.last_used = now;
      }
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
  let dir = cache_dir();
  std::fs::create_dir_all(&dir).with_context(|| {
    format!("failed to create the bundle cache dir at {}", dir.display())
  })?;

  let hash = xxh3_64(bytes);
  let dest = dir.join(format!("{hash:016x}.eszip"));
  if touch(&dest) {
    return Ok(dest);
  }

  sweep_and_make_room(bytes.len() as u64, None);
  let tmp = tmp_path(&dir, &format!("{hash:016x}"));
  std::fs::write(&tmp, bytes)
    .and_then(|_| std::fs::rename(&tmp, &dest))
    .inspect_err(|_| {
      let _ = std::fs::remove_file(&tmp);
    })
    .with_context(|| {
      format!("failed to write the bundle to {}", dest.display())
    })?;

  index_admit(&dest, bytes.len() as u64);
  Ok(dest)
}

/// A recorded eszip download: which content-addressed blob a URL's bundle
/// landed in, plus what is needed to decide whether the network can be
/// skipped next time — an explicit `version` pin, or the HTTP validators
/// (`ETag`/`Last-Modified`) for a conditional re-fetch.
///
/// The manifest key is `(cache_key ?? url, version)`: a caller that sets
/// `cache_key` asserts that all the URLs it uses with that key are
/// interchangeable views of one resource (presigned URLs rotate per request,
/// the object they sign for does not). `url` then only records where the
/// bytes last came from.
///
/// Serialized camelCase because entries travel through the
/// `op_eszip_url_cache_*` ops straight to/from JS.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UrlCacheEntry {
  #[serde(default)]
  pub url: Option<String>,
  #[serde(default)]
  pub cache_key: Option<String>,
  #[serde(default)]
  pub version: Option<String>,
  #[serde(default)]
  pub etag: Option<String>,
  #[serde(default)]
  pub last_modified: Option<String>,
  pub bundle_path: PathBuf,
}

/// Meta file for a `(key, version)` download manifest: `<xxh3-64>.url.json`
/// in the cache dir. The stored entry echoes its key so a hash collision
/// reads as a miss instead of serving the wrong bundle.
///
/// A `cache_key`-keyed manifest hashes a `\0k\0` prefix that no URL string
/// can contain (URL parsing rejects raw NULs), so an explicit key can never
/// collide with a plain-URL entry — and url-keyed manifests hash exactly as
/// before, keeping every pre-`cacheKey` cache entry valid.
fn url_meta_path(
  key: &str,
  is_cache_key: bool,
  version: Option<&str>,
) -> PathBuf {
  let mut hasher = Xxh3::default();
  if is_cache_key {
    hasher.update(b"\0k\0");
  }
  hasher.update(key.as_bytes());
  hasher.update(b"\0");
  hasher.update(version.unwrap_or("").as_bytes());
  cache_dir().join(format!("{:016x}.url.json", hasher.digest()))
}

impl UrlCacheEntry {
  /// The manifest key this entry is stored under: the explicit `cache_key`
  /// when present, the download URL otherwise.
  fn key(&self) -> Result<(&str, bool), anyhow::Error> {
    match (&self.cache_key, &self.url) {
      (Some(key), _) => Ok((key, true)),
      (None, Some(url)) => Ok((url, false)),
      (None, None) => Err(anyhow::anyhow!(
        "a url manifest entry needs a cacheKey or a url"
      )),
    }
  }
}

/// Looks up the recorded download for `(cache_key ?? url, version)`. A hit
/// touches both the meta file and the referenced bundle so the TTL sweep
/// sees them as used; a meta file whose bundle was swept (or that fails to
/// parse) is removed and reads as a miss.
pub async fn url_cache_lookup(
  url: String,
  cache_key: Option<String>,
  version: Option<String>,
) -> Result<Option<UrlCacheEntry>, anyhow::Error> {
  fs::IO_RT
    .spawn_blocking(move || {
      url_cache_lookup_blocking(&url, cache_key.as_deref(), version.as_deref())
    })
    .await
    .context("the bundle cache lookup task failed")?
}

fn url_cache_lookup_blocking(
  url: &str,
  cache_key: Option<&str>,
  version: Option<&str>,
) -> Result<Option<UrlCacheEntry>, anyhow::Error> {
  let (key, is_cache_key) = match cache_key {
    Some(key) => (key, true),
    None => (url, false),
  };
  let meta_path = url_meta_path(key, is_cache_key, version);
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
  // Echo check against a hash collision. A cache_key-keyed lookup matches on
  // the key alone (URLs are interchangeable by assertion); a url-keyed
  // lookup must match a url-keyed entry exactly.
  let matches = match cache_key {
    Some(key) => entry.cache_key.as_deref() == Some(key),
    None => entry.cache_key.is_none() && entry.url.as_deref() == Some(url),
  };
  if !matches || entry.version.as_deref() != version {
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
/// replaces any previous entry for the same `(cache_key ?? url, version)`).
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
  let (key, is_cache_key) = entry.key()?;
  let dest = url_meta_path(key, is_cache_key, entry.version.as_deref());
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

/// Moves a locally built bundle file into the content-addressed cache
/// (`FlowRuntime.bundleCache.put` with a path source): one streaming hash
/// pass — the bytes never become resident — then a rename, falling back to
/// copy+unlink across filesystems. **The source file is consumed.** Returns
/// the blob path for the manifest record that follows.
pub async fn ingest_path(path: PathBuf) -> Result<PathBuf, anyhow::Error> {
  fs::IO_RT
    .spawn_blocking(move || ingest_path_blocking(&path))
    .await
    .context("the bundle cache ingest task failed")?
}

fn ingest_path_blocking(src: &Path) -> Result<PathBuf, anyhow::Error> {
  use std::io::Read;

  let dir = cache_dir();
  std::fs::create_dir_all(&dir).with_context(|| {
    format!("failed to create the bundle cache dir at {}", dir.display())
  })?;

  let mut file = std::fs::File::open(src).with_context(|| {
    format!("failed to open the bundle at {}", src.display())
  })?;
  let mut hasher = Xxh3::default();
  let mut buf = vec![0u8; 64 * 1024];
  let mut size = 0u64;
  loop {
    let n = file
      .read(&mut buf)
      .with_context(|| format!("failed to read {}", src.display()))?;
    if n == 0 {
      break;
    }
    hasher.update(&buf[..n]);
    size += n as u64;
  }
  drop(file);

  let dest = dir.join(format!("{:016x}.eszip", hasher.digest()));
  if touch(&dest) {
    // An identical blob is already cached; the source is consumed anyway so
    // put() has uniform move semantics.
    let _ = std::fs::remove_file(src);
    return Ok(dest);
  }

  sweep_and_make_room(size, None);
  if std::fs::rename(src, &dest).is_err() {
    // Cross-device (or exotic) rename: stage a copy next to the destination
    // so the final publish stays atomic, then consume the source.
    let tmp = tmp_path(&dir, "ingest");
    std::fs::copy(src, &tmp)
      .map_err(anyhow::Error::from)
      .and_then(|_| Ok(std::fs::rename(&tmp, &dest)?))
      .inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
      })
      .with_context(|| {
        format!("failed to move the bundle to {}", dest.display())
      })?;
    let _ = std::fs::remove_file(src);
  }
  index_admit(&dest, size);
  Ok(dest)
}

/// Removes the manifest recorded for `(cache_key, version)` and — when no
/// other manifest references it — unlinks the backing blob
/// (`FlowRuntime.bundleCache.evict`). Returns whether a manifest was
/// removed. Live workers are unaffected (their open fd keeps the parse
/// serving on Unix); new creates for the key miss and re-download.
pub async fn url_cache_evict(
  cache_key: String,
  version: Option<String>,
) -> Result<bool, anyhow::Error> {
  fs::IO_RT
    .spawn_blocking(move || {
      url_cache_evict_blocking(&cache_key, version.as_deref())
    })
    .await
    .context("the bundle cache evict task failed")?
}

fn url_cache_evict_blocking(
  cache_key: &str,
  version: Option<&str>,
) -> Result<bool, anyhow::Error> {
  let meta_path = url_meta_path(cache_key, true, version);
  let bytes = match std::fs::read(&meta_path) {
    Ok(bytes) => bytes,
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
    Err(e) => {
      return Err(e).with_context(|| {
        format!("failed to read the url manifest at {}", meta_path.display())
      });
    }
  };

  let entry = serde_json::from_slice::<UrlCacheEntry>(&bytes).ok();
  match &entry {
    // Corrupt manifest: clean it up, but it wasn't this key's entry.
    None => {
      let _ = std::fs::remove_file(&meta_path);
      return Ok(false);
    }
    // Hash collision with someone else's entry: leave it alone.
    Some(other)
      if other.cache_key.as_deref() != Some(cache_key)
        || other.version.as_deref() != version =>
    {
      return Ok(false);
    }
    Some(_) => {}
  }
  let entry = entry.expect("checked above");

  std::fs::remove_file(&meta_path).with_context(|| {
    format!(
      "failed to remove the url manifest at {}",
      meta_path.display()
    )
  })?;

  if !blob_has_other_references(&entry.bundle_path, &meta_path) {
    let size = std::fs::metadata(&entry.bundle_path)
      .map(|it| it.len())
      .unwrap_or(0);
    match std::fs::remove_file(&entry.bundle_path) {
      Ok(()) => {
        index_remove(&entry.bundle_path);
        log::info!(
          "flow bundle cache: evicted {} (explicit)",
          entry.bundle_path.display()
        );
        let index = index().lock().unwrap();
        let total_bytes = index.total_bytes;
        drop(index);
        emit(CacheEvent {
          action: CacheEventAction::Evicted,
          cache_key: Some(cache_key.to_string()),
          path: Some(entry.bundle_path.clone()),
          bytes: size,
          total_bytes,
          max_bytes: max_cache_size(),
        });
      }
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
        index_remove(&entry.bundle_path);
      }
      // Windows refuses to unlink an open file; the TTL/LRU sweeps retry.
      Err(e) => {
        log::debug!(
          "flow bundle cache: could not unlink {}: {e}",
          entry.bundle_path.display()
        );
      }
    }
  }
  Ok(true)
}

/// Whether any manifest other than `excluding` points at `blob` — identical
/// bundle bytes stored under several keys share one content-addressed file.
fn blob_has_other_references(blob: &Path, excluding: &Path) -> bool {
  let Ok(entries) = std::fs::read_dir(cache_dir()) else {
    return false;
  };
  entries.flatten().any(|entry| {
    let path = entry.path();
    path != excluding
      && path.extension().and_then(|it| it.to_str()) == Some("json")
      && std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<UrlCacheEntry>(&bytes).ok())
        .is_some_and(|other| other.bundle_path == blob)
  })
}

/// A point-in-time view of the blob store (`FlowRuntime.bundleCache.stats`).
/// Serialized camelCase straight to JS.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleCacheStats {
  pub total_bytes: u64,
  pub entry_count: u64,
  pub max_bytes: Option<u64>,
  pub pinned_bytes: u64,
}

pub async fn stats() -> Result<BundleCacheStats, anyhow::Error> {
  fs::IO_RT
    .spawn_blocking(stats_blocking)
    .await
    .context("the bundle cache stats task failed")
}

fn stats_blocking() -> BundleCacheStats {
  // First call builds the index with a plain readdir (no sweeping — stats
  // stays read-only); afterwards admissions/hits keep it current.
  let entries: Vec<(PathBuf, u64)> = {
    let mut index = index().lock().unwrap();
    if !index.built {
      index.built = true;
      if let Ok(dir_entries) = std::fs::read_dir(cache_dir()) {
        for entry in dir_entries.flatten() {
          let path = entry.path();
          if path.extension().and_then(|it| it.to_str()) != Some("eszip") {
            continue;
          }
          let Ok(metadata) = entry.metadata() else {
            continue;
          };
          index.entries.insert(
            path,
            IndexEntry {
              size: metadata.len(),
              last_used: metadata.modified().unwrap_or(SystemTime::now()),
            },
          );
          index.total_bytes += metadata.len();
        }
      }
    }
    index
      .entries
      .iter()
      .map(|(path, entry)| (path.clone(), entry.size))
      .collect()
  };

  let pinned_bytes = entries
    .iter()
    .filter(|(path, _)| is_pinned(path))
    .map(|(_, size)| size)
    .sum();
  BundleCacheStats {
    total_bytes: entries.iter().map(|(_, size)| size).sum(),
    entry_count: entries.len() as u64,
    max_bytes: max_cache_size(),
    pinned_bytes,
  }
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
  /// `size_hint` is the expected bundle size when the caller knows it (a
  /// download's `Content-Length`): room is made under the cache cap before
  /// the bytes start landing. The true size is reconciled in
  /// [`Self::finish`] either way.
  pub async fn create(size_hint: Option<u64>) -> Result<Self, anyhow::Error> {
    fs::IO_RT
      .spawn_blocking(move || {
        let dir = cache_dir();
        std::fs::create_dir_all(&dir).with_context(|| {
          format!("failed to create the bundle cache dir at {}", dir.display())
        })?;
        sweep_and_make_room(size_hint.unwrap_or(0), None);
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
        let size = file
          .metadata()
          .context("failed to stat the spill file")?
          .len();
        drop(file);

        let dest = cache_dir().join(format!("{:016x}.eszip", hasher.digest()));
        if !touch(&dest) {
          std::fs::rename(&tmp, &dest).with_context(|| {
            format!("failed to store the bundle at {}", dest.display())
          })?;
          index_admit(&dest, size);
          // Reconcile when the size wasn't known (or was wrong) up front:
          // evict below the cap again, never the blob just admitted.
          if max_cache_size()
            .is_some_and(|cap| index().lock().unwrap().total_bytes > cap)
          {
            sweep_and_make_room(0, Some(&dest));
          }
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

    let url = "https://example.com/app.eszip";
    let entry = UrlCacheEntry {
      url: Some(url.into()),
      cache_key: None,
      version: None,
      etag: Some("\"abc\"".into()),
      last_modified: None,
      bundle_path: bundle_path.clone(),
    };

    // Miss before anything is recorded.
    assert!(
      url_cache_lookup_blocking(url, None, None)
        .unwrap()
        .is_none()
    );

    // Record → hit, with the validators intact.
    url_cache_record_blocking(&entry).unwrap();
    let hit = url_cache_lookup_blocking(url, None, None).unwrap().unwrap();
    assert_eq!(hit.bundle_path, bundle_path);
    assert_eq!(hit.etag.as_deref(), Some("\"abc\""));

    // A versioned entry for the same url is a distinct key.
    assert!(
      url_cache_lookup_blocking(url, None, Some("1.0.0"))
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
      url_cache_lookup_blocking(url, None, Some("1.0.0"))
        .unwrap()
        .is_some()
    );
    // ... and re-recording it replaces rather than duplicates.
    url_cache_record_blocking(&versioned).unwrap();

    // The unversioned entry is still there and independent.
    assert!(
      url_cache_lookup_blocking(url, None, None)
        .unwrap()
        .is_some()
    );

    // cacheKey-keyed entries live in their own namespace: a key equal to the
    // URL string neither hits nor clobbers the url-keyed entry, and the URLs
    // recorded under a cacheKey are interchangeable (lookup matches under a
    // different url).
    assert!(
      url_cache_lookup_blocking(url, Some(url), None)
        .unwrap()
        .is_none()
    );
    let keyed = UrlCacheEntry {
      cache_key: Some(url.into()),
      etag: None,
      ..entry.clone()
    };
    url_cache_record_blocking(&keyed).unwrap();
    let keyed_hit = url_cache_lookup_blocking(
      "https://other.example/presigned?sig=1",
      Some(url),
      None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(keyed_hit.bundle_path, bundle_path);
    let url_hit = url_cache_lookup_blocking(url, None, None).unwrap().unwrap();
    assert!(url_hit.cache_key.is_none());

    // Old-format manifests (no cacheKey field) still parse and hit.
    let legacy = url_meta_path("https://legacy.example/app.eszip", false, None);
    std::fs::write(
      &legacy,
      format!(
        r#"{{"url":"https://legacy.example/app.eszip","bundlePath":{}}}"#,
        serde_json::to_string(&bundle_path).unwrap()
      ),
    )
    .unwrap();
    assert!(
      url_cache_lookup_blocking("https://legacy.example/app.eszip", None, None)
        .unwrap()
        .is_some()
    );

    // A swept-away bundle turns the entry into a miss and drops the meta.
    std::fs::remove_file(&bundle_path).unwrap();
    assert!(
      url_cache_lookup_blocking(url, None, None)
        .unwrap()
        .is_none()
    );
    assert!(!url_meta_path(url, false, None).exists());

    // Corrupt meta reads as a miss and is cleaned up.
    let meta = url_meta_path(url, false, Some("2.0.0"));
    std::fs::write(&meta, b"{ not json").unwrap();
    assert!(
      url_cache_lookup_blocking(url, None, Some("2.0.0"))
        .unwrap()
        .is_none()
    );
    assert!(!meta.exists());

    // --- bundleCache.put/evict + LRU coverage ---

    // ingest_path: the source is MOVED into the content-addressed store.
    let src = dir.path().join("freshly-built-bundle");
    std::fs::write(&src, b"bundle bytes one").unwrap();
    let blob_a = ingest_path_blocking(&src).unwrap();
    assert!(!src.exists(), "put(path) consumes the source");
    assert!(blob_a.exists());
    assert!(blob_a.extension().is_some_and(|it| it == "eszip"));

    // Identical content converges on the same blob (source still consumed).
    std::fs::write(&src, b"bundle bytes one").unwrap();
    assert_eq!(ingest_path_blocking(&src).unwrap(), blob_a);
    assert!(!src.exists());

    // Two manifests share the blob: evicting one keeps it (another
    // reference exists), evicting the last drops it.
    for key in ["k1", "k2"] {
      url_cache_record_blocking(&UrlCacheEntry {
        url: None,
        cache_key: Some(key.into()),
        version: Some("1".into()),
        etag: None,
        last_modified: None,
        bundle_path: blob_a.clone(),
      })
      .unwrap();
    }
    assert!(url_cache_evict_blocking("k1", Some("1")).unwrap());
    assert!(blob_a.exists(), "a shared blob survives one eviction");
    assert!(
      !url_cache_evict_blocking("k1", Some("1")).unwrap(),
      "evict is idempotent"
    );
    assert!(url_cache_evict_blocking("k2", Some("1")).unwrap());
    assert!(!blob_a.exists(), "the last reference drops the blob");

    // LRU: oldest-by-mtime unpinned blobs go first, until the cap fits.
    let mk = |name: &str, len: usize, age_secs: u64| {
      let path = dir.path().join(name);
      std::fs::write(&path, vec![0u8; len]).unwrap();
      let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
      file
        .set_modified(SystemTime::now() - Duration::from_secs(age_secs))
        .unwrap();
      path
    };
    let oldest = mk("aaaaaaaaaaaaaaaa.eszip", 400, 3000);
    let middle = mk("bbbbbbbbbbbbbbbb.eszip", 400, 2000);
    let newest = mk("cccccccccccccccc.eszip", 400, 1000);
    // SAFETY: same single-test-fn reasoning as FLOW_BUNDLE_CACHE_DIR above.
    unsafe {
      std::env::set_var("FLOW_BUNDLE_CACHE_MAX_SIZE", "900");
    }
    // 1200 cached > 900: only the oldest must go.
    sweep_and_make_room(0, None);
    assert!(!oldest.exists() && middle.exists() && newest.exists());
    // 800 cached + 400 incoming > 900: the next-oldest goes too.
    sweep_and_make_room(400, None);
    assert!(!middle.exists() && newest.exists());
    // SAFETY: same single-test-fn reasoning as FLOW_BUNDLE_CACHE_DIR above.
    unsafe {
      std::env::remove_var("FLOW_BUNDLE_CACHE_MAX_SIZE");
    }

    // stats reflect the index the sweeps just rebuilt.
    let stats = stats_blocking();
    assert_eq!(stats.entry_count, 1);
    assert_eq!(stats.total_bytes, 400);
    assert_eq!(stats.max_bytes, None);
    assert_eq!(stats.pinned_bytes, 0);
  }
}
