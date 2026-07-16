//! flow: the `FlowRuntime.bundle` / `FlowRuntime.unbundle` backend —
//! programmatic eszip bundling and extraction for the flow main isolate,
//! mirroring the `flow eszip bundle` / `flow eszip unbundle` CLI.
//!
//! Bundling is CPU-heavy and built on thread-affine machinery (the
//! `EmitterFactory` graph tooling is `!Send`), so `op_eszip_bundle` runs it on
//! a dedicated thread with its own current-thread tokio runtime and hands the
//! finished bytes back through a oneshot channel. The op itself is synchronous
//! and cheap: it returns the resource id of a byte stream that JS wraps with
//! `readableStreamForRid`; awaiting the first read is what awaits the bundle.
//!
//! Unbundling walks the archive with `deno_facade::EszipEntryReader` — the
//! same enumeration `flow eszip unbundle` writes to disk — one entry per
//! `op_eszip_unbundle_next` call. Writing extracted files to disk is left to
//! the JS side (via `Deno.writeFile`), so `flow run`'s permission model
//! applies to the output directory.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Error;
use anyhow::anyhow;
use anyhow::bail;
use base::CacheSetting;
use base::WorkerKind;
use base::get_default_permissions;
use deno_core::AsyncRefCell;
use deno_core::AsyncResult;
use deno_core::BufView;
use deno_core::FastString;
use deno_core::JsBuffer;
use deno_core::OpState;
use deno_core::RcRef;
use deno_core::Resource;
use deno_core::ResourceId;
use deno_core::ToJsBuffer;
use deno_core::op2;
use deno_error::JsErrorBox;
use deno_facade::Checksum;
use deno_facade::DenoOptionsBuilder;
use deno_facade::EmitterFactory;
use deno_facade::EszipEntryReader;
use deno_facade::EszipPayloadKind;
use deno_facade::Metadata;
use deno_facade::bundle_cache::SpillFile;
use deno_facade::bundle_cache::UrlCacheEntry;
use deno_facade::generate_binary_eszip;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::oneshot;
use tokio::time::timeout;

fn generic_err(err: impl std::fmt::Display) -> JsErrorBox {
  JsErrorBox::generic(format!("{err:#}"))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct EszipBundleOptions {
  /// Path of the entry module on disk. Mutually exclusive with `module_code`.
  entrypoint: Option<String>,
  /// Source code of the entry module (the `FlowRuntime.bundle(buffer)` form).
  module_code: Option<String>,
  static_patterns: Vec<String>,
  /// "none" | "sha256" | "xxhash3" (matches `flow eszip bundle --checksum`).
  checksum: Option<String>,
  timeout_ms: Option<u64>,
  no_module_cache: bool,
  import_map_path: Option<String>,
  /// Specifiers or globs whose module subtree is left out of the bundle (each
  /// match becomes a bare import resolved at runtime). Deps shared with a
  /// non-excluded module stay bundled.
  exclude: Vec<String>,
}

/// Maps the JS-facing checksum names onto the same `Checksum::from_u8`
/// discriminants `flow eszip bundle --checksum` uses.
fn parse_checksum(value: Option<&str>) -> Result<Option<Checksum>, JsErrorBox> {
  let discriminant = match value {
    None => return Ok(None),
    Some("none") => 0,
    Some("sha256") => 1,
    Some("xxhash3") => 2,
    Some(other) => {
      return Err(JsErrorBox::type_error(format!(
        "invalid checksum kind: {other} (expected \"none\", \"sha256\" or \
         \"xxhash3\")"
      )));
    }
  };
  Ok(Checksum::from_u8(discriminant))
}

async fn bundle_eszip(
  options: EszipBundleOptions,
  maybe_checksum: Option<Checksum>,
) -> Result<Vec<u8>, Error> {
  let mut emitter_factory = EmitterFactory::new();
  if options.no_module_cache {
    emitter_factory.set_cache_strategy(Some(CacheSetting::ReloadAll));
  }
  emitter_factory.set_permissions_options(Some(get_default_permissions(
    WorkerKind::MainWorker,
  )));

  let mut builder = DenoOptionsBuilder::new();
  let maybe_code = if let Some(entrypoint) = options.entrypoint.as_deref() {
    let entrypoint_path = PathBuf::from(entrypoint);
    if !entrypoint_path.is_file() {
      bail!(
        "entrypoint path does not exist ({})",
        entrypoint_path.display()
      );
    }
    builder.set_entrypoint(Some(entrypoint_path.canonicalize()?));
    None
  } else {
    // Bundle in-memory source instead; `generate_binary_eszip` gives it the
    // synthetic `/src/index.ts` path (same as code-only user workers).
    options.module_code.map(FastString::from)
  };
  builder.set_import_map_path(options.import_map_path);
  emitter_factory.set_deno_options(builder.build().await?);

  let static_patterns: Vec<&str> =
    options.static_patterns.iter().map(|s| s.as_str()).collect();

  let mut metadata = Metadata::default();
  #[allow(
    clippy::arc_with_non_send_sync,
    reason = "eszip generation runs on this thread only; the Arc-wrapped \
              factory never crosses threads"
  )]
  let eszip_fut = generate_binary_eszip(
    &mut metadata,
    Arc::new(emitter_factory),
    maybe_code,
    maybe_checksum,
    Some(static_patterns),
    Some(options.exclude),
  );

  let eszip = match options.timeout_ms.map(Duration::from_millis) {
    Some(dur) => timeout(dur, eszip_fut).await.map_err(|_| {
      anyhow!("failed to complete the bundle within the given time")
    })??,
    None => eszip_fut.await?,
  };

  Ok(eszip.into_bytes())
}

enum BundleStreamState {
  /// Bundle still being generated on its thread.
  Pending(oneshot::Receiver<Result<Vec<u8>, String>>),
  /// Bundle finished; serving `data[pos..]` to readers.
  Streaming {
    data: Vec<u8>,
    pos: usize,
  },
  Done,
}

/// The `FlowRuntime.bundle` result as a deno_core byte-stream resource
/// (consumed via `readableStreamForRid` on the JS side).
struct EszipBundleStreamResource {
  state: AsyncRefCell<BundleStreamState>,
}

impl Resource for EszipBundleStreamResource {
  fn name(&self) -> std::borrow::Cow<'_, str> {
    "eszipBundleStream".into()
  }

  fn read(self: Rc<Self>, limit: usize) -> AsyncResult<BufView> {
    Box::pin(async move {
      let mut state = RcRef::map(&self, |r| &r.state).borrow_mut().await;
      let (data, pos) =
        match std::mem::replace(&mut *state, BundleStreamState::Done) {
          BundleStreamState::Pending(rx) => match rx.await {
            Ok(Ok(data)) => (data, 0),
            Ok(Err(msg)) => return Err(JsErrorBox::generic(msg)),
            Err(_) => {
              return Err(JsErrorBox::generic(
                "the eszip bundling thread exited unexpectedly",
              ));
            }
          },
          BundleStreamState::Streaming { data, pos } => (data, pos),
          BundleStreamState::Done => return Ok(BufView::empty()),
        };

      if pos >= data.len() {
        return Ok(BufView::empty());
      }
      let len = std::cmp::min(limit, data.len() - pos);
      let chunk = data[pos..pos + len].to_vec();
      *state = BundleStreamState::Streaming {
        data,
        pos: pos + len,
      };
      Ok(BufView::from(chunk))
    })
  }
}

/// Starts bundling on a dedicated thread and returns the rid of the byte
/// stream serving the finished eszip. Bundle failures surface as read errors
/// on that stream.
#[op2]
#[smi]
fn op_eszip_bundle(
  state: &mut OpState,
  #[serde] options: EszipBundleOptions,
) -> Result<ResourceId, JsErrorBox> {
  if options.entrypoint.is_none() && options.module_code.is_none() {
    return Err(JsErrorBox::type_error(
      "either an entrypoint path or module code must be provided",
    ));
  }
  // Parsed here so an unknown kind throws from `bundle()` itself, not as a
  // later stream error.
  let maybe_checksum = parse_checksum(options.checksum.as_deref())?;

  let (result_tx, result_rx) = oneshot::channel();
  std::thread::Builder::new()
    .name("flow-eszip-bundle".into())
    .spawn(move || {
      let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build the eszip bundling runtime");
      let local = tokio::task::LocalSet::new();
      let result =
        local.block_on(&runtime, bundle_eszip(options, maybe_checksum));
      // The receiver is gone when the stream was closed early; nothing to do.
      let _ = result_tx.send(result.map_err(|e| format!("{e:#}")));
    })
    .map_err(|e| {
      generic_err(format!("failed to spawn bundling thread: {e}"))
    })?;

  Ok(state.resource_table.add(EszipBundleStreamResource {
    state: AsyncRefCell::new(BundleStreamState::Pending(result_rx)),
  }))
}

/// An open unbundle job: the entry walker behind `FlowRuntime.unbundle`.
struct EszipUnbundleResource {
  reader: AsyncRefCell<EszipEntryReader>,
}

impl Resource for EszipUnbundleResource {
  fn name(&self) -> std::borrow::Cow<'_, str> {
    "eszipUnbundle".into()
  }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EszipUnbundleSource {
  /// Path of an .eszip file on disk. Mutually exclusive with `data`.
  path: Option<String>,
  /// The eszip bytes themselves.
  data: Option<JsBuffer>,
}

/// Parses an eszip archive (from a path or bytes) and returns the rid of the
/// entry walker consumed by `op_eszip_unbundle_next`.
#[op2]
#[smi]
async fn op_eszip_unbundle_open(
  state: Rc<RefCell<OpState>>,
  #[serde] source: EszipUnbundleSource,
) -> Result<ResourceId, JsErrorBox> {
  let payload = match (source.path, source.data) {
    (Some(path), None) => EszipPayloadKind::VecKind(
      tokio::fs::read(&path)
        .await
        .map_err(|e| generic_err(format!("failed to read {path}: {e}")))?,
    ),
    (None, Some(data)) => EszipPayloadKind::JsBufferKind(data),
    _ => {
      return Err(JsErrorBox::type_error(
        "exactly one of an eszip path or eszip bytes must be provided",
      ));
    }
  };

  let reader = EszipEntryReader::open(payload).await.map_err(generic_err)?;
  Ok(
    state
      .borrow_mut()
      .resource_table
      .add(EszipUnbundleResource {
        reader: AsyncRefCell::new(reader),
      }),
  )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EszipUnbundleEntry {
  specifier: String,
  /// Destination path relative to the extraction root.
  path: String,
  /// "module" | "static" | "vfs"
  kind: &'static str,
  data: ToJsBuffer,
}

/// Yields the next entry of an open unbundle job, or `null` once all entries
/// have been read.
#[op2]
#[serde]
async fn op_eszip_unbundle_next(
  state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
) -> Result<Option<EszipUnbundleEntry>, JsErrorBox> {
  let resource = state
    .borrow()
    .resource_table
    .get::<EszipUnbundleResource>(rid)
    .map_err(generic_err)?;

  let mut reader = RcRef::map(&resource, |r| &r.reader).borrow_mut().await;
  let entry = reader.next_entry().await.map_err(generic_err)?;

  Ok(entry.map(|entry| EszipUnbundleEntry {
    specifier: entry.specifier,
    path: entry.relative_path.to_string_lossy().into_owned(),
    kind: entry.kind.as_str(),
    data: Vec::from(entry.data.as_ref()).into(),
  }))
}

/// An in-progress spill of a streamed `maybeEszip` into the bundle cache
/// (see `createUserWorker` in flow_main.js). Closing the resource before
/// `op_eszip_spill_finish` drops the [`SpillFile`], which unlinks its temp
/// file.
struct EszipSpillResource {
  spill: AsyncRefCell<Option<SpillFile>>,
}

impl Resource for EszipSpillResource {
  fn name(&self) -> std::borrow::Cow<'_, str> {
    "eszipSpill".into()
  }
}

/// Opens a spill file in the bundle cache and returns its resource id.
/// `size_hint` (e.g. a download's `Content-Length`; `0` = unknown) lets the
/// cache make room under its size cap before the bytes start landing.
#[op2]
#[smi]
async fn op_eszip_spill_open(
  state: Rc<RefCell<OpState>>,
  #[number] size_hint: u64,
) -> Result<ResourceId, JsErrorBox> {
  let spill = SpillFile::create((size_hint > 0).then_some(size_hint))
    .await
    .map_err(generic_err)?;
  Ok(state.borrow_mut().resource_table.add(EszipSpillResource {
    spill: AsyncRefCell::new(Some(spill)),
  }))
}

/// Appends a chunk to an open spill.
#[op2]
async fn op_eszip_spill_write(
  state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
  #[buffer] chunk: JsBuffer,
) -> Result<(), JsErrorBox> {
  let resource = state
    .borrow()
    .resource_table
    .get::<EszipSpillResource>(rid)
    .map_err(generic_err)?;
  let mut guard = RcRef::map(&resource, |r| &r.spill).borrow_mut().await;
  match (*guard).as_mut() {
    Some(spill) => spill.write(Vec::from(&*chunk)).await.map_err(generic_err),
    None => Err(generic_err("the spill is already finished")),
  }
}

/// Finalizes a spill into its content-addressed `<hash>.eszip` cache path and
/// returns that path (for `maybeEszipPath`). Removes the resource.
#[op2]
#[string]
async fn op_eszip_spill_finish(
  state: Rc<RefCell<OpState>>,
  #[smi] rid: ResourceId,
) -> Result<String, JsErrorBox> {
  let resource = state
    .borrow_mut()
    .resource_table
    .take::<EszipSpillResource>(rid)
    .map_err(generic_err)?;
  let mut guard = RcRef::map(&resource, |r| &r.spill).borrow_mut().await;
  let spill = (*guard)
    .take()
    .ok_or_else(|| generic_err("the spill is already finished"))?;
  drop(guard);
  let path = spill.finish().await.map_err(generic_err)?;
  Ok(path.to_string_lossy().into_owned())
}

/// Looks up the bundle-cache manifest entry recorded for a
/// `(cacheKey ?? url, version)` eszip download (see `fetchEszipUrl` in
/// flow_main.js). `None` means the URL must be (re-)downloaded.
#[op2]
#[serde]
async fn op_eszip_url_cache_lookup(
  #[string] url: String,
  #[string] cache_key: Option<String>,
  #[string] version: Option<String>,
) -> Result<Option<UrlCacheEntry>, JsErrorBox> {
  deno_facade::bundle_cache::url_cache_lookup(url, cache_key, version)
    .await
    .map_err(generic_err)
}

/// Records a finished `(cacheKey ?? url, version)` download in the
/// bundle-cache manifest.
#[op2]
async fn op_eszip_url_cache_record(
  #[serde] entry: UrlCacheEntry,
) -> Result<(), JsErrorBox> {
  deno_facade::bundle_cache::url_cache_record(entry)
    .await
    .map_err(generic_err)
}

/// Moves a locally built bundle file into the content-addressed cache and
/// returns the blob path (`FlowRuntime.bundleCache.put` with a path source;
/// the source file is consumed).
#[op2]
#[string]
async fn op_eszip_cache_ingest_path(
  #[string] path: String,
) -> Result<String, JsErrorBox> {
  let dest = deno_facade::bundle_cache::ingest_path(PathBuf::from(path))
    .await
    .map_err(generic_err)?;
  Ok(dest.to_string_lossy().into_owned())
}

/// Drops the manifest for `(cacheKey, version)` and its blob when nothing
/// else references it (`FlowRuntime.bundleCache.evict`). Returns whether an
/// entry was removed.
#[op2]
async fn op_eszip_url_cache_evict(
  #[string] cache_key: String,
  #[string] version: Option<String>,
) -> Result<bool, JsErrorBox> {
  deno_facade::bundle_cache::url_cache_evict(cache_key, version)
    .await
    .map_err(generic_err)
}

/// Point-in-time bundle-cache numbers (`FlowRuntime.bundleCache.stats`).
#[op2]
#[serde]
async fn op_eszip_cache_stats()
-> Result<deno_facade::bundle_cache::BundleCacheStats, JsErrorBox> {
  deno_facade::bundle_cache::stats()
    .await
    .map_err(generic_err)
}

deno_core::extension!(
  // flow: OPS-ONLY for the same reason as `user_workers_ops` - an ESM-bearing
  // extension can't link against Deno's CLI snapshot. The JS surface
  // (`FlowRuntime.bundle`/`unbundle`) is installed post-bootstrap by
  // flow_main.js, and these op names must stay in the `NOT_IMPORTED_OPS`
  // allowlist in runtime/js/99_main.js.
  flow_eszip_ops,
  ops = [
    op_eszip_bundle,
    op_eszip_unbundle_open,
    op_eszip_unbundle_next,
    op_eszip_spill_open,
    op_eszip_spill_write,
    op_eszip_spill_finish,
    op_eszip_url_cache_lookup,
    op_eszip_url_cache_record,
    op_eszip_cache_ingest_path,
    op_eszip_url_cache_evict,
    op_eszip_cache_stats
  ],
);
