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
        Some("eszip") => bundle_ttl(),
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
