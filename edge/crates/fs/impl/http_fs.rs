//! HttpFS — a virtual filesystem backed by any HTTP API implementing the
//! HttpFS Protocol v1 (see `edge/docs/httpfs-protocol.md`). The runtime's sole
//! consumer-facing surface is `deno_fs::FileSystem`, mounted into user workers
//! behind a `PrefixFs` layer, so every path this module sees is
//! mount-relative.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io;
use std::mem;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::ensure;
use bytes::Bytes;
use deno_core::AsyncRefCell;
use deno_core::BufMutView;
use deno_core::BufView;
use deno_core::RcRef;
use deno_core::ResourceHandleFd;
use deno_core::WriteOutcome;
use deno_fs::FsDirEntry;
use deno_fs::FsFileType;
use deno_fs::OpenOptions;
use deno_io::fs::File;
use deno_io::fs::FsError;
use deno_io::fs::FsResult;
use deno_io::fs::FsStat;
use deno_permissions::CheckedPath;
use deno_permissions::CheckedPathBuf;
use enum_as_inner::EnumAsInner;
use futures::AsyncReadExt;
use futures::FutureExt;
use futures::TryFutureExt;
use futures::TryStreamExt;
use futures::future::BoxFuture;
use futures::future::LocalBoxFuture;
use futures::future::Shared;
use http_body_util::BodyExt;
use http_body_util::Full;
use hyper_util::rt::TokioIo;
use indexmap::IndexMap;
use reqwest::Method;
use reqwest::StatusCode;
use reqwest::header;
use serde::Deserialize;
use serde::Serialize;
use tokio::task::JoinError;
use tracing::debug;
use tracing::error;
use tracing::instrument;
use tracing::trace;
use tracing::warn;
use url::Position;
use url::Url;

use crate::rt;

/// The protocol major version this client speaks. Servers advertising a
/// different `version` in `GET /capabilities` are refused.
pub const HTTPFS_PROTOCOL_VERSION: u64 = 1;

/// How long `stat`/`list` responses are trusted before the server is asked
/// again. Kept sub-second per protocol §6; every mutation through this client
/// invalidates the whole cache (write-through).
const META_CACHE_TTL: Duration = Duration::from_millis(500);

/// Delay before the single retry of an idempotent request that answered
/// 5xx/429.
const RETRY_DELAY: Duration = Duration::from_millis(200);

type BackgroundTask = Shared<BoxFuture<'static, Result<(), Arc<JoinError>>>>;

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HttpFsConfig {
  /// Unlike S3 mounts there is no default: every HttpFS mount names its own
  /// mount point.
  mount_point: String,
  base_url: String,
  /// Custom headers attached to every request (e.g. `Authorization`,
  /// `X-CSRF-Token`). Auth is the caller's concern, not the protocol's.
  #[serde(default)]
  headers: IndexMap<String, String>,
  /// Custom query params appended to every request (protocol §1.1).
  #[serde(default)]
  query: IndexMap<String, String>,
  /// When set, requests are made over this AF_UNIX socket instead of TCP.
  /// `baseUrl` still supplies the URL path prefix, the `Host` header, and the
  /// origin that scopes credentials across redirects — use a placeholder host
  /// (e.g. `http://localhost/fs/v1`). Cross-origin redirect/presigned targets
  /// are still fetched over TCP.
  #[serde(default)]
  socket_path: Option<String>,
}

/// One or many HttpFS mounts (the `httpFs` worker create option).
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(untagged)]
pub enum HttpFsConfigs {
  One(HttpFsConfig),
  Many(Vec<HttpFsConfig>),
}

impl HttpFsConfigs {
  pub fn into_vec(self) -> Vec<HttpFsConfig> {
    match self {
      Self::One(config) => vec![config],
      Self::Many(configs) => configs,
    }
  }
}

impl From<HttpFsConfig> for HttpFsConfigs {
  fn from(config: HttpFsConfig) -> Self {
    Self::One(config)
  }
}

impl HttpFsConfig {
  pub fn take_mount_point(&mut self) -> String {
    mem::take(&mut self.mount_point)
  }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct Capabilities {
  version: u64,
  /// 0 = unlimited.
  #[serde(default)]
  direct_write_max_bytes: u64,
  multipart: Option<MultipartCaps>,
  #[serde(default)]
  copy: bool,
  max_file_bytes: Option<u64>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct MultipartCaps {
  min_part_bytes: u64,
  max_part_bytes: u64,
  max_parts: u64,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum EntryKind {
  File,
  Dir,
}

/// Protocol §3.1 `Entry`.
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct Entry {
  path: String,
  kind: EntryKind,
  #[serde(default)]
  size: u64,
  mtime_ms: Option<u64>,
  birthtime_ms: Option<u64>,
}

impl Entry {
  fn name(&self) -> &str {
    self.path.rsplit('/').next().unwrap_or_default()
  }

  fn into_stat(self) -> FsStat {
    let is_file = self.kind == EntryKind::File;
    FsStat {
      is_file,
      is_directory: !is_file,
      is_symlink: false,
      size: if is_file { self.size } else { 0 },
      mtime: self.mtime_ms,
      atime: None,
      birthtime: self.birthtime_ms,
      ctime: None,
      dev: 0,
      ino: Some(0),
      mode: 0,
      nlink: Some(0),
      uid: 0,
      gid: 0,
      rdev: 0,
      blksize: 0,
      blocks: Some(0),
      is_block_device: false,
      is_char_device: false,
      is_fifo: false,
      is_socket: false,
    }
  }
}

#[derive(Deserialize, Debug)]
struct ListResponse {
  entries: Vec<Entry>,
  cursor: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
struct ErrorBody {
  code: Option<String>,
  message: Option<String>,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct FromToBody<'a> {
  from: &'a str,
  to: &'a str,
  overwrite: bool,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct UploadInit {
  upload_id: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct PartGrant {
  url: String,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct CommitPart {
  part_number: u64,
  #[serde(skip_serializing_if = "Option::is_none")]
  etag: Option<String>,
}

#[derive(Serialize, Debug)]
struct CommitBody {
  parts: Vec<CommitPart>,
}

#[derive(Default)]
struct MetaCache {
  stats: HashMap<String, (Instant, Entry)>,
  lists: HashMap<String, (Instant, Vec<Entry>)>,
}

#[derive(Debug, Clone)]
pub struct HttpFs {
  inner: Arc<HttpFsInner>,
}

struct HttpFsInner {
  client: reqwest::Client,
  base_url: Url,
  /// When `Some`, protocol requests to `base_url`'s origin are driven over this
  /// AF_UNIX socket instead of TCP (see [`HttpFsConfig::socket_path`]). The
  /// `client` is still used to *build* every request and to reach cross-origin
  /// (presigned) targets over TCP.
  socket_path: Option<PathBuf>,
  /// Custom headers attached to every same-origin request. Pre-parsed so an
  /// invalid name/value fails at mount creation, not on the first fs op.
  headers: header::HeaderMap,
  /// Custom query pairs appended to every same-origin request.
  query: Vec<(String, String)>,
  capabilities: tokio::sync::OnceCell<Capabilities>,
  cache: Mutex<MetaCache>,
  /// Flush tasks spawned by dropped file handles (see [`HttpObject`]'s `Drop`
  /// impl). They run on [`rt::IO_RT`], so they survive worker teardown —
  /// nothing in the worker driver needs to flush this fs.
  background_tasks: Mutex<Vec<BackgroundTask>>,
}

impl std::fmt::Debug for HttpFsInner {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("HttpFsInner")
      .field("base_url", &self.base_url.as_str())
      .finish_non_exhaustive()
  }
}

impl HttpFs {
  pub fn new(config: HttpFsConfig) -> Result<Self, anyhow::Error> {
    let base_url = Url::parse(&config.base_url).with_context(|| {
      format!("invalid HttpFS baseUrl: {}", config.base_url)
    })?;

    // `baseUrl` stays an http(s) URL even for unix-socket mounts: it supplies
    // the path prefix, the `Host` header, and the origin used to scope
    // credentials across redirects (use a placeholder host like
    // `http://localhost/fs/v1`). The socket path, when set, only replaces the
    // TCP transport for same-origin requests.
    ensure!(
      matches!(base_url.scheme(), "http" | "https"),
      "HttpFS baseUrl must be http(s): {}",
      config.base_url
    );
    ensure!(
      base_url.host_str().is_some(),
      "HttpFS baseUrl must have a host: {}",
      config.base_url
    );

    let headers = config
      .headers
      .iter()
      .map(|(name, value)| {
        let name = header::HeaderName::try_from(name.as_str())
          .with_context(|| format!("invalid HttpFS header name: {name}"))?;
        let value = header::HeaderValue::try_from(value.as_str())
          .with_context(|| {
            format!("invalid HttpFS header value for: {name}")
          })?;
        Ok((name, value))
      })
      .collect::<Result<header::HeaderMap, anyhow::Error>>()?;

    let client = reqwest::Client::builder()
      // Redirects are handled manually so custom headers/query can be stripped
      // from cross-origin targets (presigned URLs); reqwest only strips a fixed
      // set of sensitive headers, not custom ones like `X-Api-Key`.
      .redirect(reqwest::redirect::Policy::none())
      // Requests are driven from several runtimes (the worker's runtime for
      // async fs calls, IO_RT for sync calls and drop-flush tasks). A pooled
      // connection is owned by the runtime that first drove it, so reusing it
      // from another runtime can hang after the owner shuts down — disable
      // keep-alive reuse instead.
      .pool_max_idle_per_host(0)
      .build()
      .context("failed to build the HttpFS HTTP client")?;

    Ok(Self {
      inner: Arc::new(HttpFsInner {
        client,
        base_url,
        socket_path: config.socket_path.map(PathBuf::from),
        headers,
        query: config.query.into_iter().collect(),
        capabilities: tokio::sync::OnceCell::new(),
        cache: Mutex::default(),
        background_tasks: Mutex::default(),
      }),
    })
  }

  pub async fn flush_background_tasks(&self) {
    self.inner.flush_background_tasks().await;
  }

  async fn open_inner(
    &self,
    path: PathBuf,
    options: OpenOptions,
  ) -> FsResult<Rc<HttpObject>> {
    self.inner.flush_background_tasks().await;

    let path = protocol_path(&path)?;

    if path == "/" {
      return Err(FsError::Io(io::Error::from(io::ErrorKind::IsADirectory)));
    }

    let existing = match self.inner.stat_entry(&path).await {
      Ok(entry) => {
        if entry.kind == EntryKind::Dir {
          return Err(FsError::Io(io::Error::from(
            io::ErrorKind::IsADirectory,
          )));
        }
        if options.create_new {
          return Err(FsError::Io(io::Error::from(
            io::ErrorKind::AlreadyExists,
          )));
        }
        Some(entry)
      }
      Err(err) if err.kind() == io::ErrorKind::NotFound => {
        if !(options.create || options.create_new) {
          return Err(FsError::Io(io::Error::from(io::ErrorKind::NotFound)));
        }
        None
      }
      Err(err) => return Err(err),
    };

    let file = Rc::new(HttpObject {
      fs: self.clone(),
      path,
      op_slot: AsyncRefCell::default(),
    });

    if existing.is_none() || options.truncate {
      // Lazily buffer the (empty) content; it is flushed as one `PUT /write`
      // on sync/close, so a plain `writeFile` costs a single round trip.
      // `overwrite=false` backs O_EXCL server-side against create races.
      file
        .begin_buffered_write(Vec::new(), !options.create_new)
        .await;
    } else if options.append {
      // True append semantics: seed the write buffer with the existing
      // content so the flush rewrites the whole object.
      let resp = self.inner.read_response(&file.path, None).await?;
      let resp = ensure_success(resp).await?;
      let data = resp.bytes().await.map_err(io::Error::other)?.to_vec();
      file.begin_buffered_write(data, true).await;
    }

    Ok(file)
  }
}

impl HttpFsInner {
  async fn flush_background_tasks(&self) {
    loop {
      let tasks = mem::take(&mut *self.background_tasks.lock().unwrap());
      if tasks.is_empty() {
        break;
      }
      for task in tasks {
        let _ = task.await;
      }
    }
  }

  fn invalidate_cache(&self) {
    let mut cache = self.cache.lock().unwrap();
    cache.stats.clear();
    cache.lists.clear();
  }

  fn cached_stat(&self, path: &str) -> Option<Entry> {
    let cache = self.cache.lock().unwrap();
    let (at, entry) = cache.stats.get(path)?;
    (at.elapsed() < META_CACHE_TTL).then(|| entry.clone())
  }

  fn cached_list(&self, path: &str) -> Option<Vec<Entry>> {
    let cache = self.cache.lock().unwrap();
    let (at, entries) = cache.lists.get(path)?;
    (at.elapsed() < META_CACHE_TTL).then(|| entries.clone())
  }

  /// Builds a request against a protocol endpoint, attaching the protocol
  /// query `params`, the caller's custom query pairs, and the custom headers.
  fn request(
    &self,
    method: Method,
    endpoint: &str,
    params: &[(&str, &str)],
  ) -> reqwest::RequestBuilder {
    let mut url = self.base_url.clone();

    {
      let path = format!("{}{endpoint}", url.path().trim_end_matches('/'));
      url.set_path(&path);
    }

    {
      let mut pairs = url.query_pairs_mut();
      for (name, value) in params {
        pairs.append_pair(name, value);
      }
      for (name, value) in &self.query {
        pairs.append_pair(name, value);
      }
    }

    self.apply_headers(self.client.request(method, url))
  }

  /// Attaches the caller's custom headers. Applied per-request (never as client
  /// defaults) so cross-origin redirect/presigned targets never inherit them.
  fn apply_headers(
    &self,
    req: reqwest::RequestBuilder,
  ) -> reqwest::RequestBuilder {
    req.headers(self.headers.clone())
  }

  /// Whether a request to `base_url`'s own origin should go over the unix
  /// socket (as opposed to TCP). Cross-origin targets always take TCP, so
  /// callers touching redirect/presigned URLs pass an explicit flag to the
  /// `*_dispatch` variants instead.
  fn over_socket_default(&self) -> bool {
    self.socket_path.is_some()
  }

  async fn send(
    &self,
    req: reqwest::RequestBuilder,
  ) -> FsResult<reqwest::Response> {
    self.send_dispatch(req, self.over_socket_default()).await
  }

  /// Sends over the unix socket when `over_socket`, else over TCP via reqwest.
  async fn send_dispatch(
    &self,
    req: reqwest::RequestBuilder,
    over_socket: bool,
  ) -> FsResult<reqwest::Response> {
    if over_socket {
      let socket = self
        .socket_path
        .as_deref()
        .expect("over_socket implies a configured socket_path");
      let req = req
        .build()
        .map_err(|err| FsError::Io(io::Error::other(err)))?;
      unix_send(socket, req).await
    } else {
      req
        .send()
        .await
        .map_err(|err| FsError::Io(io::Error::other(err)))
    }
  }

  /// Sends a request that is safe to repeat, retrying once on 5xx/429
  /// (protocol §6).
  async fn send_idempotent(
    &self,
    req: reqwest::RequestBuilder,
  ) -> FsResult<reqwest::Response> {
    self
      .send_idempotent_dispatch(req, self.over_socket_default())
      .await
  }

  async fn send_idempotent_dispatch(
    &self,
    req: reqwest::RequestBuilder,
    over_socket: bool,
  ) -> FsResult<reqwest::Response> {
    let retry = req.try_clone();
    let resp = self.send_dispatch(req, over_socket).await?;

    let should_retry = resp.status().is_server_error()
      || resp.status() == StatusCode::TOO_MANY_REQUESTS;

    match (should_retry, retry) {
      (true, Some(retry)) => {
        debug!(status = %resp.status(), "retrying idempotent HttpFS request");
        tokio::time::sleep(RETRY_DELAY).await;
        self.send_dispatch(retry, over_socket).await
      }

      _ => Ok(resp),
    }
  }

  /// Fetches `GET /capabilities` once per mount and refuses protocol-major
  /// mismatches (protocol §4).
  async fn capabilities(&self) -> FsResult<Capabilities> {
    self
      .capabilities
      .get_or_try_init(|| async {
        let resp = self
          .send_idempotent(self.request(Method::GET, "/capabilities", &[]))
          .await?;
        let resp = ensure_success(resp).await?;
        let caps = resp
          .json::<Capabilities>()
          .await
          .map_err(|err| FsError::Io(io::Error::other(err)))?;

        if caps.version != HTTPFS_PROTOCOL_VERSION {
          return Err(FsError::Io(io::Error::other(format!(
            "server speaks HttpFS protocol version {}, this client requires {}",
            caps.version, HTTPFS_PROTOCOL_VERSION
          ))));
        }

        Ok(caps)
      })
      .await
      .cloned()
  }

  /// `GET /read`, following at most one redirect. The credential is attached
  /// to the redirect target only when it shares `baseUrl`'s origin; a
  /// cross-origin target (e.g. a presigned URL) is fetched bare
  /// (protocol §1.1/§5.3). The response status is NOT checked here.
  async fn read_response(
    &self,
    path: &str,
    range: Option<(u64, Option<u64>)>,
  ) -> FsResult<reqwest::Response> {
    fn apply_range(
      req: reqwest::RequestBuilder,
      range: Option<(u64, Option<u64>)>,
    ) -> reqwest::RequestBuilder {
      match range {
        Some((start, Some(end))) => {
          req.header(header::RANGE, format!("bytes={start}-{end}"))
        }
        Some((start, None)) => {
          req.header(header::RANGE, format!("bytes={start}-"))
        }
        None => req,
      }
    }

    let req =
      apply_range(self.request(Method::GET, "/read", &[("path", path)]), range);
    let resp = self.send_idempotent(req).await?;

    if !resp.status().is_redirection() {
      return Ok(resp);
    }

    let location = resp
      .headers()
      .get(header::LOCATION)
      .and_then(|it| it.to_str().ok())
      .ok_or_else(|| {
        FsError::Io(io::Error::other("HttpFS read redirect without location"))
      })?;

    let mut target = self
      .base_url
      .join(location)
      .map_err(|err| FsError::Io(io::Error::other(err)))?;
    let same_origin = target.origin() == self.base_url.origin();

    if same_origin {
      let mut pairs = target.query_pairs_mut();
      for (name, value) in &self.query {
        pairs.append_pair(name, value);
      }
    }

    let mut req = self.client.get(target);
    if same_origin {
      req = self.apply_headers(req);
    }

    // Same-origin targets ride the same transport as the base request; a
    // cross-origin presigned URL is a real host, so it always takes TCP.
    let over_socket = self.socket_path.is_some() && same_origin;
    self
      .send_idempotent_dispatch(apply_range(req, range), over_socket)
      .await
  }

  async fn stat_entry(&self, path: &str) -> FsResult<Entry> {
    self.capabilities().await?;
    self.flush_background_tasks().await;

    if let Some(entry) = self.cached_stat(path) {
      return Ok(entry);
    }

    let resp = self
      .send_idempotent(self.request(Method::GET, "/stat", &[("path", path)]))
      .await?;
    let resp = ensure_success(resp).await?;
    let entry = resp
      .json::<Entry>()
      .await
      .map_err(|err| FsError::Io(io::Error::other(err)))?;

    self
      .cache
      .lock()
      .unwrap()
      .stats
      .insert(path.to_string(), (Instant::now(), entry.clone()));

    Ok(entry)
  }

  async fn list_dir(&self, path: &str) -> FsResult<Vec<Entry>> {
    self.capabilities().await?;
    self.flush_background_tasks().await;

    if let Some(entries) = self.cached_list(path) {
      return Ok(entries);
    }

    let mut entries = Vec::new();
    let mut cursor: Option<String> = None;

    loop {
      let mut params = vec![("path", path)];
      if let Some(cursor) = cursor.as_deref() {
        params.push(("cursor", cursor));
      }

      let resp = self
        .send_idempotent(self.request(Method::GET, "/list", &params))
        .await?;
      let resp = ensure_success(resp).await?;
      let page = resp
        .json::<ListResponse>()
        .await
        .map_err(|err| FsError::Io(io::Error::other(err)))?;

      entries.extend(page.entries);

      match page.cursor {
        Some(next) if !next.is_empty() => cursor = Some(next),
        _ => break,
      }
    }

    self
      .cache
      .lock()
      .unwrap()
      .lists
      .insert(path.to_string(), (Instant::now(), entries.clone()));

    Ok(entries)
  }

  /// Writes the full object content: one `PUT /write` when it fits the
  /// server's direct-write limit, multipart otherwise (protocol §5.4/§5.5).
  async fn write_object(
    &self,
    path: &str,
    data: &[u8],
    overwrite: bool,
  ) -> FsResult<()> {
    let caps = self.capabilities().await?;

    if let Some(max) = caps.max_file_bytes
      && data.len() as u64 > max
    {
      return Err(FsError::Io(io::Error::new(
        io::ErrorKind::FileTooLarge,
        format!(
          "write of {} bytes exceeds the server's file size cap ({max})",
          data.len()
        ),
      )));
    }

    let limit = caps.direct_write_max_bytes;
    let result = if limit == 0 || data.len() as u64 <= limit {
      let mut params = vec![("path", path)];
      if !overwrite {
        params.push(("overwrite", "false"));
      }

      let req = self
        .request(Method::PUT, "/write", &params)
        .body(data.to_vec());
      ensure_success(self.send(req).await?).await.map(drop)
    } else if let Some(multipart) = caps.multipart.as_ref() {
      self.multipart_upload(path, data, multipart).await
    } else {
      Err(FsError::Io(io::Error::new(
        io::ErrorKind::FileTooLarge,
        format!(
          "write of {} bytes exceeds the server's direct-write limit \
           ({limit}) and the server does not support multipart upload",
          data.len()
        ),
      )))
    };

    self.invalidate_cache();
    result
  }

  async fn multipart_upload(
    &self,
    path: &str,
    data: &[u8],
    caps: &MultipartCaps,
  ) -> FsResult<()> {
    let len = data.len() as u64;
    let max_parts = caps.max_parts.max(1);
    let mut part_size = caps.min_part_bytes.max(1);

    if len.div_ceil(part_size) > max_parts {
      part_size = len.div_ceil(max_parts);
    }
    if caps.max_part_bytes > 0 && part_size > caps.max_part_bytes {
      return Err(FsError::Io(io::Error::new(
        io::ErrorKind::FileTooLarge,
        format!(
          "write of {len} bytes does not fit the server's multipart limits"
        ),
      )));
    }

    let size_hint = len.to_string();
    let resp = self
      .send(self.request(
        Method::POST,
        "/upload",
        &[("path", path), ("sizeHint", &size_hint)],
      ))
      .await?;
    let resp = ensure_success(resp).await?;
    let UploadInit { upload_id } = resp
      .json()
      .await
      .map_err(|err| FsError::Io(io::Error::other(err)))?;

    match self
      .multipart_upload_parts(&upload_id, data, part_size as usize)
      .await
    {
      Ok(()) => Ok(()),
      Err(err) => {
        // Abort is best-effort; uncommitted uploads are the server's to GC.
        if let Err(abort_err) = async {
          ensure_success(
            self
              .send(self.request(
                Method::DELETE,
                "/upload",
                &[("uploadId", &upload_id)],
              ))
              .await?,
          )
          .await
        }
        .await
        {
          warn!(reason = ?abort_err, "failed to abort HttpFS multipart upload");
        }

        Err(err)
      }
    }
  }

  async fn multipart_upload_parts(
    &self,
    upload_id: &str,
    data: &[u8],
    part_size: usize,
  ) -> FsResult<()> {
    let mut parts = Vec::new();

    for (idx, chunk) in data.chunks(part_size).enumerate() {
      let part_number = (idx + 1) as u64;
      let part_number_str = part_number.to_string();
      let size = chunk.len().to_string();

      let resp = self
        .send(self.request(
          Method::POST,
          "/upload/part",
          &[
            ("uploadId", upload_id),
            ("partNumber", &part_number_str),
            ("size", &size),
          ],
        ))
        .await?;
      let resp = ensure_success(resp).await?;
      let PartGrant { url } = resp
        .json()
        .await
        .map_err(|err| FsError::Io(io::Error::other(err)))?;

      let target = self
        .base_url
        .join(&url)
        .map_err(|err| FsError::Io(io::Error::other(err)))?;
      let same_origin = target.origin() == self.base_url.origin();

      let mut target = target;
      if same_origin {
        let mut pairs = target.query_pairs_mut();
        for (name, value) in &self.query {
          pairs.append_pair(name, value);
        }
      }

      let mut req = self.client.put(target);
      if same_origin {
        req = self.apply_headers(req);
      }

      let over_socket = self.socket_path.is_some() && same_origin;
      let resp = ensure_success(
        self
          .send_dispatch(req.body(chunk.to_vec()), over_socket)
          .await?,
      )
      .await?;
      let etag = resp
        .headers()
        .get(header::ETAG)
        .and_then(|it| it.to_str().ok())
        .map(str::to_owned);

      parts.push(CommitPart { part_number, etag });
    }

    let req = self
      .request(Method::POST, "/upload/commit", &[("uploadId", upload_id)])
      .json(&CommitBody { parts });

    ensure_success(self.send(req).await?).await.map(drop)
  }

  async fn mkdir(&self, path: &str, parents: bool) -> FsResult<()> {
    self.capabilities().await?;
    self.flush_background_tasks().await;

    let mut params = vec![("path", path)];
    if parents {
      params.push(("parents", "true"));
    }

    let result = ensure_success(
      self
        .send(self.request(Method::POST, "/mkdir", &params))
        .await?,
    )
    .await
    .map(drop);

    self.invalidate_cache();
    result
  }

  async fn remove(&self, path: &str, recursive: bool) -> FsResult<()> {
    self.capabilities().await?;
    self.flush_background_tasks().await;

    let mut params = vec![("path", path)];
    if recursive {
      params.push(("recursive", "true"));
    }

    let result = ensure_success(
      self
        .send(self.request(Method::DELETE, "/remove", &params))
        .await?,
    )
    .await
    .map(drop);

    self.invalidate_cache();
    result
  }

  async fn move_entry(
    &self,
    from: &str,
    to: &str,
    overwrite: bool,
  ) -> FsResult<()> {
    self.capabilities().await?;
    self.flush_background_tasks().await;

    let req = self.request(Method::POST, "/move", &[]).json(&FromToBody {
      from,
      to,
      overwrite,
    });

    let result = ensure_success(self.send(req).await?).await.map(drop);

    self.invalidate_cache();
    result
  }

  /// Copies a single file: `POST /copy` when the server declares the
  /// capability, otherwise emulated with read + write (protocol §4).
  async fn copy_file_entry(
    &self,
    from: &str,
    to: &str,
    overwrite: bool,
  ) -> FsResult<()> {
    let caps = self.capabilities().await?;
    self.flush_background_tasks().await;

    if caps.copy {
      let req = self.request(Method::POST, "/copy", &[]).json(&FromToBody {
        from,
        to,
        overwrite,
      });

      let result = ensure_success(self.send(req).await?).await.map(drop);

      self.invalidate_cache();
      result
    } else {
      let resp = ensure_success(self.read_response(from, None).await?).await?;
      let data = resp
        .bytes()
        .await
        .map_err(|err| FsError::Io(io::Error::other(err)))?;

      self.write_object(to, &data, overwrite).await
    }
  }

  fn copy_dir_recursive<'a>(
    &'a self,
    from: String,
    to: String,
  ) -> BoxFuture<'a, FsResult<()>> {
    async move {
      self.mkdir(&to, true).await?;

      for entry in self.list_dir(&from).await? {
        let name = entry.name();
        if name.is_empty() {
          continue;
        }

        let src = join_protocol_path(&from, name);
        let dst = join_protocol_path(&to, name);

        match entry.kind {
          EntryKind::File => self.copy_file_entry(&src, &dst, true).await?,
          EntryKind::Dir => self.copy_dir_recursive(src, dst).await?,
        }
      }

      Ok(())
    }
    .boxed()
  }
}

async fn ensure_success(
  resp: reqwest::Response,
) -> FsResult<reqwest::Response> {
  let status = resp.status();
  if status.is_success() {
    return Ok(resp);
  }

  let body = resp.bytes().await.unwrap_or_default();
  let parsed =
    deno_core::serde_json::from_slice::<ErrorBody>(&body).unwrap_or_default();

  let kind = match parsed.code.as_deref() {
    Some("NotFound") => io::ErrorKind::NotFound,
    Some("AlreadyExists") => io::ErrorKind::AlreadyExists,
    Some("NotADirectory") => io::ErrorKind::NotADirectory,
    Some("IsADirectory") => io::ErrorKind::IsADirectory,
    Some("NotEmpty") => io::ErrorKind::DirectoryNotEmpty,
    Some("PermissionDenied") | Some("Unauthenticated") => {
      io::ErrorKind::PermissionDenied
    }
    Some("TooLarge") => io::ErrorKind::FileTooLarge,
    Some("InvalidPath") => io::ErrorKind::InvalidInput,

    // Unknown/absent codes fall back to the HTTP status (protocol §3.2).
    _ => match status {
      StatusCode::NOT_FOUND => io::ErrorKind::NotFound,
      StatusCode::CONFLICT => io::ErrorKind::AlreadyExists,
      StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
        io::ErrorKind::PermissionDenied
      }
      StatusCode::PAYLOAD_TOO_LARGE => io::ErrorKind::FileTooLarge,
      StatusCode::BAD_REQUEST => io::ErrorKind::InvalidInput,
      _ => io::ErrorKind::Other,
    },
  };

  let message = parsed
    .message
    .unwrap_or_else(|| format!("HttpFS server answered {status}"));

  Err(FsError::Io(io::Error::new(kind, message)))
}

/// Drives one request over an AF_UNIX socket and adapts the answer back into a
/// [`reqwest::Response`], so the whole protocol layer stays transport-agnostic.
///
/// A fresh connection is opened per request (no pooling) — this mirrors the TCP
/// client's `pool_max_idle_per_host(0)` and side-steps the cross-runtime
/// connection-ownership hazard documented in [`HttpFs::new`]. The response body
/// is buffered in full rather than streamed: unix mounts target a local
/// sidecar, and the write path already buffers whole objects, so trading
/// streaming for a much smaller transport shim is the right call here.
async fn unix_send(
  socket: &Path,
  req: reqwest::Request,
) -> FsResult<reqwest::Response> {
  let stream =
    tokio::net::UnixStream::connect(socket)
      .await
      .map_err(|err| {
        FsError::Io(io::Error::new(
          err.kind(),
          format!(
            "HttpFS failed to connect to unix socket {}: {err}",
            socket.display()
          ),
        ))
      })?;

  let (mut sender, conn) = hyper::client::conn::http1::handshake::<
    _,
    Full<Bytes>,
  >(TokioIo::new(stream))
  .await
  .map_err(|err| FsError::Io(io::Error::other(err)))?;

  // The connection future must be polled for the exchange to progress; it
  // resolves on its own once the (unpooled) connection closes.
  let conn = tokio::spawn(async move {
    let _ = conn.await;
  });

  let http_req = build_http_request(req)?;
  let resp = sender
    .send_request(http_req)
    .await
    .map_err(|err| FsError::Io(io::Error::other(err)))?;
  drop(sender);

  let (parts, incoming) = resp.into_parts();
  let body = incoming
    .collect()
    .await
    .map_err(|err| FsError::Io(io::Error::other(err)))?
    .to_bytes();
  conn.abort();

  Ok(reqwest::Response::from(http::Response::from_parts(
    parts,
    reqwest::Body::from(body),
  )))
}

/// Rebuilds a built [`reqwest::Request`] as an origin-form `http::Request` for
/// hyper's client connection. Bodies here are always in memory (buffered
/// writes / JSON), so [`reqwest::Body::as_bytes`] is sufficient.
fn build_http_request(
  req: reqwest::Request,
) -> FsResult<http::Request<Full<Bytes>>> {
  let url = req.url();
  let host = url.host_str().unwrap_or("localhost");
  let authority = match url.port() {
    Some(port) => format!("{host}:{port}"),
    None => host.to_string(),
  };
  let target = &url[Position::BeforePath..];

  let mut builder = http::Request::builder()
    .method(req.method().clone())
    .uri(target);

  {
    let headers = builder
      .headers_mut()
      .expect("a freshly built request has valid parts");
    *headers = req.headers().clone();
    // reqwest only sets `Host` when it drives the request itself; we bypass
    // that, so set it from the URL's authority.
    if !headers.contains_key(http::header::HOST) {
      headers.insert(
        http::header::HOST,
        http::HeaderValue::from_str(&authority)
          .map_err(|err| FsError::Io(io::Error::other(err)))?,
      );
    }
  }

  let body = req
    .body()
    .and_then(reqwest::Body::as_bytes)
    .map(Bytes::copy_from_slice)
    .unwrap_or_default();

  builder
    .body(Full::new(body))
    .map_err(|err| FsError::Io(io::Error::other(err)))
}

/// Converts a mount-relative [`Path`] (as handed over by the `PrefixFs`
/// layer) into a normalized protocol path: absolute from the mount root,
/// `/`-separated, no `.`/`..`/empty segments (protocol §2).
fn protocol_path(path: &Path) -> FsResult<String> {
  let invalid_input =
    || FsError::Io(io::Error::from(io::ErrorKind::InvalidInput));

  let mut segments: Vec<&str> = Vec::new();

  for component in path.components() {
    match component {
      Component::RootDir | Component::CurDir => {}
      Component::ParentDir => {
        // `..` that would escape the mount root is invalid.
        if segments.pop().is_none() {
          return Err(invalid_input());
        }
      }
      Component::Normal(segment) => {
        let segment = segment.to_str().ok_or_else(invalid_input)?;
        if segment.contains('\0') {
          return Err(invalid_input());
        }
        segments.push(segment);
      }
      Component::Prefix(_) => return Err(invalid_input()),
    }
  }

  if segments.is_empty() {
    return Ok("/".to_string());
  }

  Ok(format!("/{}", segments.join("/")))
}

fn join_protocol_path(base: &str, name: &str) -> String {
  if base == "/" {
    format!("/{name}")
  } else {
    format!("{base}/{name}")
  }
}

#[async_trait::async_trait(?Send)]
impl deno_fs::FileSystem for HttpFs {
  fn cwd(&self) -> FsResult<PathBuf> {
    Err(FsError::NotSupported)
  }

  fn tmp_dir(&self) -> FsResult<PathBuf> {
    Err(FsError::NotSupported)
  }

  fn chdir(&self, _path: &CheckedPath) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  fn umask(&self, _mask: Option<u32>) -> FsResult<u32> {
    Err(FsError::NotSupported)
  }

  fn open_sync(
    &self,
    path: &CheckedPath,
    options: OpenOptions,
  ) -> FsResult<Rc<dyn File>> {
    let file_ptr = std::thread::scope(|s| {
      let path = path.to_path_buf();
      s.spawn(move || {
        rt::IO_RT.block_on(async move {
          self.open_inner(path, options).await.map(|it| {
            // SAFETY: the pointer only crosses back to the parent thread,
            // which is blocked on join() until this closure finishes.
            unsafe { it.into_ptr() }
          })
        })
      })
      .join()
      .unwrap()
    })?;

    // SAFETY: reconstructs the Rc leaked by into_ptr on the scoped thread,
    // so it is reclaimed exactly once.
    Ok(unsafe { HttpObject::from_ptr(file_ptr) })
  }

  #[instrument(
    level = "trace",
    skip(self, options),
    fields(?options),
    err(Debug)
  )]
  async fn open_async<'a>(
    &'a self,
    path: CheckedPathBuf,
    options: OpenOptions,
  ) -> FsResult<Rc<dyn File>> {
    Ok(self.open_inner(path.to_path_buf(), options).await?)
  }

  fn mkdir_sync(
    &self,
    path: &CheckedPath,
    recursive: bool,
    mode: Option<u32>,
  ) -> FsResult<()> {
    std::thread::scope(|s| {
      let path = path.to_path_buf();
      s.spawn(move || {
        rt::IO_RT.block_on(async move {
          self
            .mkdir_async(CheckedPathBuf::unsafe_new(path), recursive, mode)
            .await
        })
      })
      .join()
      .unwrap()
    })
  }

  #[instrument(level = "trace", skip(self, _mode), fields(mode = _mode) ret, err(Debug))]
  async fn mkdir_async(
    &self,
    path: CheckedPathBuf,
    recursive: bool,
    _mode: Option<u32>,
  ) -> FsResult<()> {
    let path = protocol_path(&path)?;

    if path == "/" {
      return Err(FsError::Io(io::Error::from(io::ErrorKind::AlreadyExists)));
    }

    self.inner.mkdir(&path, recursive).await
  }

  #[cfg(unix)]
  fn chmod_sync(&self, _path: &CheckedPath, _mode: u32) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  #[cfg(not(unix))]
  fn chmod_sync(&self, _path: &CheckedPath, _mode: i32) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  #[cfg(unix)]
  async fn chmod_async(
    &self,
    _path: CheckedPathBuf,
    _mode: u32,
  ) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  #[cfg(not(unix))]
  async fn chmod_async(
    &self,
    _path: CheckedPathBuf,
    _mode: i32,
  ) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  #[cfg(unix)]
  fn lchmod_sync(&self, _path: &CheckedPath, _mode: u32) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  #[cfg(not(unix))]
  fn lchmod_sync(&self, _path: &CheckedPath, _mode: i32) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  #[cfg(unix)]
  async fn lchmod_async(
    &self,
    _path: CheckedPathBuf,
    _mode: u32,
  ) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  #[cfg(not(unix))]
  async fn lchmod_async(
    &self,
    _path: CheckedPathBuf,
    _mode: i32,
  ) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  fn chown_sync(
    &self,
    _path: &CheckedPath,
    _uid: Option<u32>,
    _gid: Option<u32>,
  ) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  async fn chown_async(
    &self,
    _path: CheckedPathBuf,
    _uid: Option<u32>,
    _gid: Option<u32>,
  ) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  fn lchown_sync(
    &self,
    _path: &CheckedPath,
    _uid: Option<u32>,
    _gid: Option<u32>,
  ) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  async fn lchown_async(
    &self,
    _path: CheckedPathBuf,
    _uid: Option<u32>,
    _gid: Option<u32>,
  ) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  fn remove_sync(&self, path: &CheckedPath, recursive: bool) -> FsResult<()> {
    std::thread::scope(|s| {
      let path = path.to_path_buf();
      s.spawn(move || {
        rt::IO_RT.block_on(async move {
          self
            .remove_async(CheckedPathBuf::unsafe_new(path), recursive)
            .await
        })
      })
      .join()
      .unwrap()
    })
  }

  #[instrument(level = "trace", skip(self), ret, err(Debug))]
  async fn remove_async(
    &self,
    path: CheckedPathBuf,
    recursive: bool,
  ) -> FsResult<()> {
    let path = protocol_path(&path)?;
    self.inner.remove(&path, recursive).await
  }

  fn rmdir_sync(&self, _path: &CheckedPath) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  async fn rmdir_async(&self, _path: CheckedPathBuf) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  fn copy_file_sync(
    &self,
    oldpath: &CheckedPath,
    newpath: &CheckedPath,
  ) -> FsResult<()> {
    std::thread::scope(|s| {
      let oldpath = oldpath.to_path_buf();
      let newpath = newpath.to_path_buf();
      s.spawn(move || {
        rt::IO_RT.block_on(async move {
          self
            .copy_file_async(
              CheckedPathBuf::unsafe_new(oldpath),
              CheckedPathBuf::unsafe_new(newpath),
            )
            .await
        })
      })
      .join()
      .unwrap()
    })
  }

  #[instrument(level = "trace", skip(self), ret, err(Debug))]
  async fn copy_file_async(
    &self,
    oldpath: CheckedPathBuf,
    newpath: CheckedPathBuf,
  ) -> FsResult<()> {
    let from = protocol_path(&oldpath)?;
    let to = protocol_path(&newpath)?;

    self.inner.copy_file_entry(&from, &to, true).await
  }

  fn cp_sync(
    &self,
    path: &CheckedPath,
    new_path: &CheckedPath,
  ) -> FsResult<()> {
    std::thread::scope(|s| {
      let path = path.to_path_buf();
      let new_path = new_path.to_path_buf();
      s.spawn(move || {
        rt::IO_RT.block_on(async move {
          self
            .cp_async(
              CheckedPathBuf::unsafe_new(path),
              CheckedPathBuf::unsafe_new(new_path),
            )
            .await
        })
      })
      .join()
      .unwrap()
    })
  }

  #[instrument(level = "trace", skip(self), ret, err(Debug))]
  async fn cp_async(
    &self,
    path: CheckedPathBuf,
    new_path: CheckedPathBuf,
  ) -> FsResult<()> {
    let from = protocol_path(&path)?;
    let to = protocol_path(&new_path)?;

    match self.inner.stat_entry(&from).await?.kind {
      EntryKind::File => self.inner.copy_file_entry(&from, &to, true).await,
      EntryKind::Dir => self.inner.copy_dir_recursive(from, to).await,
    }
  }

  fn stat_sync(&self, path: &CheckedPath) -> FsResult<FsStat> {
    std::thread::scope(|s| {
      let path = path.to_path_buf();
      s.spawn(move || {
        rt::IO_RT.block_on(async move {
          self.stat_async(CheckedPathBuf::unsafe_new(path)).await
        })
      })
      .join()
      .unwrap()
    })
  }

  #[instrument(level = "trace", skip(self), err(Debug))]
  async fn stat_async(&self, path: CheckedPathBuf) -> FsResult<FsStat> {
    let path = protocol_path(&path)?;
    Ok(self.inner.stat_entry(&path).await?.into_stat())
  }

  fn lstat_sync(&self, path: &CheckedPath) -> FsResult<FsStat> {
    std::thread::scope(|s| {
      let path = path.to_path_buf();
      s.spawn(move || {
        rt::IO_RT.block_on(async move {
          self.lstat_async(CheckedPathBuf::unsafe_new(path)).await
        })
      })
      .join()
      .unwrap()
    })
  }

  #[instrument(level = "trace", skip(self), err(Debug))]
  async fn lstat_async(&self, path: CheckedPathBuf) -> FsResult<FsStat> {
    self.stat_async(path).await
  }

  fn exists_sync(&self, path: &CheckedPath) -> bool {
    std::thread::scope(|s| {
      let path = path.to_path_buf();
      s.spawn(move || {
        rt::IO_RT.block_on(async move {
          self
            .exists_async(CheckedPathBuf::unsafe_new(path))
            .await
            .unwrap_or(false)
        })
      })
      .join()
      .unwrap()
    })
  }

  async fn exists_async(&self, path: CheckedPathBuf) -> FsResult<bool> {
    let path = protocol_path(&path)?;
    Ok(self.inner.stat_entry(&path).await.is_ok())
  }

  fn realpath_sync(&self, path: &CheckedPath) -> FsResult<PathBuf> {
    std::thread::scope(|s| {
      let path = path.to_path_buf();
      s.spawn(move || {
        rt::IO_RT.block_on(async move {
          self.realpath_async(CheckedPathBuf::unsafe_new(path)).await
        })
      })
      .join()
      .unwrap()
    })
  }

  /// HttpFS has no symlinks, so realpath is the normalized identity — rooted
  /// so the enclosing PrefixFs layer can re-join its mount prefix. Missing
  /// paths error with NotFound, matching `fs.realpath` semantics.
  #[instrument(level = "trace", skip(self), err(Debug))]
  async fn realpath_async(&self, path: CheckedPathBuf) -> FsResult<PathBuf> {
    let path = protocol_path(&path)?;

    self.inner.stat_entry(&path).await?;

    Ok(PathBuf::from(path))
  }

  fn read_dir_sync(&self, path: &CheckedPath) -> FsResult<Vec<FsDirEntry>> {
    std::thread::scope(|s| {
      let path = path.to_path_buf();
      s.spawn(move || {
        rt::IO_RT.block_on(async move {
          let rd = self
            .read_dir_async(CheckedPathBuf::unsafe_new(path))
            .await?;
          let mut entries = Vec::new();
          while let Some(entry) = deno_fs::FsReadDir::next(&*rd).await? {
            entries.push(entry);
          }
          Ok(entries)
        })
      })
      .join()
      .unwrap()
    })
  }

  #[instrument(level = "trace", skip(self), err(Debug))]
  async fn read_dir_async(
    &self,
    path: CheckedPathBuf,
  ) -> FsResult<deno_fs::FsReadDirRc> {
    let path = protocol_path(&path)?;
    let entries = self
      .inner
      .list_dir(&path)
      .await?
      .into_iter()
      .filter_map(|entry| {
        let name = entry.name().to_owned();
        if name.is_empty() {
          return None;
        }

        let is_file = entry.kind == EntryKind::File;
        Some(FsDirEntry {
          name,
          is_file,
          is_directory: !is_file,
          is_symlink: false,
        })
      })
      .collect::<Vec<_>>();

    trace!(len = entries.len());
    Ok(crate::VecFsReadDir::new_rc(entries))
  }

  fn statfs_sync(
    &self,
    _path: &CheckedPath,
    _bigint: bool,
  ) -> FsResult<deno_io::fs::FsStatFs> {
    Err(FsError::NotSupported)
  }

  async fn statfs_async(
    &self,
    _path: CheckedPathBuf,
    _bigint: bool,
  ) -> FsResult<deno_io::fs::FsStatFs> {
    Err(FsError::NotSupported)
  }

  fn rename_sync(
    &self,
    oldpath: &CheckedPath,
    newpath: &CheckedPath,
  ) -> FsResult<()> {
    std::thread::scope(|s| {
      let oldpath = oldpath.to_path_buf();
      let newpath = newpath.to_path_buf();
      s.spawn(move || {
        rt::IO_RT.block_on(async move {
          self
            .rename_async(
              CheckedPathBuf::unsafe_new(oldpath),
              CheckedPathBuf::unsafe_new(newpath),
            )
            .await
        })
      })
      .join()
      .unwrap()
    })
  }

  #[instrument(level = "trace", skip(self), ret, err(Debug))]
  async fn rename_async(
    &self,
    oldpath: CheckedPathBuf,
    newpath: CheckedPathBuf,
  ) -> FsResult<()> {
    let from = protocol_path(&oldpath)?;
    let to = protocol_path(&newpath)?;

    self.inner.move_entry(&from, &to, true).await
  }

  fn link_sync(
    &self,
    _oldpath: &CheckedPath,
    _newpath: &CheckedPath,
  ) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  async fn link_async(
    &self,
    _oldpath: CheckedPathBuf,
    _newpath: CheckedPathBuf,
  ) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  fn symlink_sync(
    &self,
    _oldpath: &CheckedPath,
    _newpath: &CheckedPath,
    _file_type: Option<FsFileType>,
  ) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  async fn symlink_async(
    &self,
    _oldpath: CheckedPathBuf,
    _newpath: CheckedPathBuf,
    _file_type: Option<FsFileType>,
  ) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  fn read_link_sync(&self, _path: &CheckedPath) -> FsResult<PathBuf> {
    Err(FsError::NotSupported)
  }

  async fn read_link_async(&self, _path: CheckedPathBuf) -> FsResult<PathBuf> {
    Err(FsError::NotSupported)
  }

  fn truncate_sync(&self, path: &CheckedPath, len: u64) -> FsResult<()> {
    std::thread::scope(|s| {
      let path = path.to_path_buf();
      s.spawn(move || {
        rt::IO_RT.block_on(async move {
          self
            .truncate_async(CheckedPathBuf::unsafe_new(path), len)
            .await
        })
      })
      .join()
      .unwrap()
    })
  }

  /// The protocol has no truncate endpoint, so this is emulated: fetch the
  /// prefix (or zero-pad past EOF) and rewrite the object.
  #[instrument(level = "trace", skip(self), ret, err(Debug))]
  async fn truncate_async(
    &self,
    path: CheckedPathBuf,
    len: u64,
  ) -> FsResult<()> {
    let path = protocol_path(&path)?;
    let entry = self.inner.stat_entry(&path).await?;

    if entry.kind == EntryKind::Dir {
      return Err(FsError::Io(io::Error::from(io::ErrorKind::IsADirectory)));
    }

    if len == 0 {
      return self.inner.write_object(&path, &[], true).await;
    }

    let len = usize::try_from(len)
      .map_err(|_| FsError::Io(io::Error::from(io::ErrorKind::InvalidInput)))?;

    let range = (entry.size > len as u64).then(|| (0, Some(len as u64 - 1)));
    let resp =
      ensure_success(self.inner.read_response(&path, range).await?).await?;
    let mut data = resp
      .bytes()
      .await
      .map_err(|err| FsError::Io(io::Error::other(err)))?
      .to_vec();

    data.resize(len, 0);

    self.inner.write_object(&path, &data, true).await
  }

  fn utime_sync(
    &self,
    _path: &CheckedPath,
    _atime_secs: i64,
    _atime_nanos: u32,
    _mtime_secs: i64,
    _mtime_nanos: u32,
  ) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  async fn utime_async(
    &self,
    _path: CheckedPathBuf,
    _atime_secs: i64,
    _atime_nanos: u32,
    _mtime_secs: i64,
    _mtime_nanos: u32,
  ) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  fn lutime_sync(
    &self,
    _path: &CheckedPath,
    _atime_secs: i64,
    _atime_nanos: u32,
    _mtime_secs: i64,
    _mtime_nanos: u32,
  ) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  async fn lutime_async(
    &self,
    _path: CheckedPathBuf,
    _atime_secs: i64,
    _atime_nanos: u32,
    _mtime_secs: i64,
    _mtime_nanos: u32,
  ) -> FsResult<()> {
    Err(FsError::NotSupported)
  }
}

#[derive(EnumAsInner)]
enum HttpObjectOpSlot {
  Read(HttpReadState),
  Write(HttpWriteState),
}

/// A streaming read: the (boxed) body reader plus how many bytes were
/// consumed, so a broken stream can resume with a ranged request.
struct HttpReadState(Pin<Box<dyn futures::io::AsyncRead + Send>>, u64);

/// Buffered write state; the whole buffer becomes the object's content on
/// flush (one `PUT /write` or a multipart upload).
struct HttpWriteState {
  buf: Vec<u8>,
  dirty: bool,
  /// `false` when the handle was opened with O_EXCL — the flush passes
  /// `overwrite=false` so the server enforces exclusivity.
  overwrite: bool,
}

pub struct HttpObjectPtr(*const HttpObject);

// SAFETY: the pointer is only ever handed to a scoped thread that the owning
// thread joins before touching the Rc again, so the (non-atomic) refcount is
// never accessed from two threads at once.
unsafe impl Send for HttpObjectPtr {}

pub struct HttpObject {
  fs: HttpFs,
  path: String,
  op_slot: AsyncRefCell<Option<HttpObjectOpSlot>>,
}

impl Drop for HttpObject {
  fn drop(&mut self) {
    let Some(HttpObjectOpSlot::Write(state)) =
      Rc::new(mem::take(&mut self.op_slot))
        .try_borrow_mut()
        .unwrap()
        .take()
    else {
      return;
    };

    if !state.dirty {
      return;
    }

    let fs = self.fs.clone();
    let path = mem::take(&mut self.path);
    let inner = fs.inner.clone();

    let task = rt::IO_RT
      .spawn(async move {
        if let Err(err) =
          inner.write_object(&path, &state.buf, state.overwrite).await
        {
          error!(reason = ?err, path, "background flush failed");
        }
      })
      .map_err(Arc::new)
      .boxed()
      .shared();

    fs.inner.background_tasks.lock().unwrap().push(task);
  }
}

impl HttpObject {
  unsafe fn into_ptr(self: Rc<Self>) -> HttpObjectPtr {
    HttpObjectPtr(Rc::into_raw(self))
  }

  unsafe fn from_ptr(ptr: HttpObjectPtr) -> Rc<Self> {
    // SAFETY: per this function's contract, ptr came from into_ptr and is
    // reconstructed at most once.
    unsafe { Rc::from_raw(ptr.0) }
  }

  /// Seeds the write slot; the buffer is flushed as the object's full content
  /// on sync/close.
  async fn begin_buffered_write(
    self: &Rc<Self>,
    buf: Vec<u8>,
    overwrite: bool,
  ) {
    let mut op_slot = RcRef::map(self, |r| &r.op_slot).borrow_mut().await;
    *op_slot = Some(HttpObjectOpSlot::Write(HttpWriteState {
      buf,
      dirty: true,
      overwrite,
    }));
  }

  fn read_byob_inner(
    self: Rc<Self>,
    buf: &mut [u8],
  ) -> LocalBoxFuture<'_, FsResult<usize>> {
    async move {
      let mut op_slot = RcRef::map(&self, |r| &r.op_slot).borrow_mut().await;
      let Some(op_slot_mut) = op_slot.as_mut() else {
        let resp = self.fs.inner.read_response(&self.path, None).await?;
        let mut reader = match ensure_success(resp).await {
          Ok(resp) => Box::pin(
            resp
              .bytes_stream()
              .map_err(io::Error::other)
              .into_async_read(),
          ),
          // Reading a handle whose object vanished yields EOF (S3Fs parity).
          Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
          Err(err) => return Err(err),
        };

        let nread = reader.read(buf).await?;

        *op_slot =
          Some(HttpObjectOpSlot::Read(HttpReadState(reader, nread as u64)));

        trace!(nread);
        return Ok(nread);
      };

      let Some(state) = op_slot_mut.as_read_mut() else {
        return Err(
          io::Error::other("read operation was blocked by another operation")
            .into(),
        );
      };

      let err = match state.0.read(buf).await {
        Ok(nread) => {
          state.1 += nread as u64;

          if nread == 0 {
            op_slot.take();
          }

          trace!(nread);
          return Ok(nread);
        }

        Err(err) => err,
      };

      let is_retryable = {
        use io::ErrorKind as E;
        matches!(
          err.kind(),
          E::ConnectionRefused
            | E::ConnectionReset
            | E::ConnectionAborted
            | E::BrokenPipe
            | E::TimedOut
            | E::NotConnected
        )
      };

      warn!(kind = %err.kind(), reason = ?err, "stream closed abnormally");
      debug!(is_retryable);

      if is_retryable {
        let resp = self
          .fs
          .inner
          .read_response(&self.path, Some((state.1, None)))
          .await?;
        let resp = ensure_success(resp).await?;
        let mut reader = Box::pin(
          resp
            .bytes_stream()
            .map_err(io::Error::other)
            .into_async_read(),
        );

        let nread = reader.read(buf).await?;

        state.1 += nread as u64;
        state.0 = reader;

        trace!(nread);
        Ok(nread)
      } else {
        op_slot.take();
        Err(io::Error::other(err).into())
      }
    }
    .boxed_local()
  }

  fn write_inner(
    self: Rc<Self>,
    buf: &[u8],
  ) -> LocalBoxFuture<'_, FsResult<usize>> {
    async move {
      // Fetched here so size-cap violations (and a protocol-version
      // mismatch) surface at write time — the background flush that
      // ultimately uploads the buffer can only log them.
      let caps = self.fs.inner.capabilities().await?;

      let mut op_slot = RcRef::map(&self, |r| &r.op_slot).borrow_mut().await;

      let total_len = match op_slot.as_mut() {
        None => {
          *op_slot = Some(HttpObjectOpSlot::Write(HttpWriteState {
            buf: buf.to_vec(),
            dirty: true,
            overwrite: true,
          }));

          buf.len() as u64
        }

        Some(slot) => {
          let Some(state) = slot.as_write_mut() else {
            return Err(
              io::Error::other(
                "write operation was blocked by another operation",
              )
              .into(),
            );
          };

          state.buf.extend_from_slice(buf);
          state.dirty = true;
          state.buf.len() as u64
        }
      };

      let over_cap =
        matches!(caps.max_file_bytes, Some(max) if total_len > max);
      let over_direct_limit = caps.direct_write_max_bytes > 0
        && total_len > caps.direct_write_max_bytes
        && caps.multipart.is_none();

      if over_cap || over_direct_limit {
        return Err(FsError::Io(io::Error::new(
          io::ErrorKind::FileTooLarge,
          format!(
            "write of {total_len} bytes exceeds what the HttpFS server \
             accepts"
          ),
        )));
      }

      trace!(nwritten = buf.len());
      Ok(buf.len())
    }
    .boxed_local()
  }
}

#[async_trait::async_trait(?Send)]
impl deno_io::fs::File for HttpObject {
  fn read_sync(self: Rc<Self>, buf: &mut [u8]) -> FsResult<usize> {
    // SAFETY: current thread will be blocked by join()
    let ptr = unsafe { self.into_ptr() };
    std::thread::scope(|s| {
      s.spawn(move || {
        rt::IO_RT.block_on(async move {
          // SAFETY: reconstructs the Rc leaked by into_ptr above, on the
          // scoped thread the parent is joining on, so it is reclaimed
          // exactly once.
          unsafe { HttpObject::from_ptr(ptr) }
            .read_byob_inner(buf)
            .await
        })
      })
      .join()
      .unwrap()
    })
  }

  #[instrument(level = "trace", skip_all, fields(self.path), err(Debug))]
  async fn read_byob(
    self: Rc<Self>,
    mut buf: BufMutView,
  ) -> FsResult<(usize, BufMutView)> {
    self.read_byob_inner(&mut buf).await.map(|it| (it, buf))
  }

  fn write_sync(self: Rc<Self>, buf: &[u8]) -> FsResult<usize> {
    // SAFETY: current thread will be blocked by join()
    let ptr = unsafe { self.into_ptr() };
    std::thread::scope(|s| {
      s.spawn(move || {
        rt::IO_RT.block_on(async move {
          // SAFETY: reconstructs the Rc leaked by into_ptr above, on the
          // scoped thread the parent is joining on, so it is reclaimed
          // exactly once.
          unsafe { HttpObject::from_ptr(ptr) }.write_inner(buf).await
        })
      })
      .join()
      .unwrap()
    })
  }

  #[instrument(level = "trace", skip_all, fields(self.path, len = buf.len()), err(Debug))]
  async fn write(self: Rc<Self>, buf: BufView) -> FsResult<WriteOutcome> {
    let nwritten = self.write_inner(&buf).await?;
    Ok(WriteOutcome::Full { nwritten })
  }

  fn write_all_sync(self: Rc<Self>, buf: &[u8]) -> FsResult<()> {
    self.write_sync(buf).map(drop)
  }

  #[instrument(level = "trace", skip_all, fields(self.path), err(Debug))]
  async fn write_all(self: Rc<Self>, buf: BufView) -> FsResult<()> {
    self.write_inner(&buf).await.map(drop)
  }

  fn read_all_sync(self: Rc<Self>) -> FsResult<Cow<'static, [u8]>> {
    // SAFETY: current thread will be blocked by join()
    let ptr = unsafe { self.into_ptr() };
    std::thread::scope(|s| {
      s.spawn(move || {
        rt::IO_RT.block_on(async move {
          // SAFETY: reconstructs the Rc leaked by into_ptr above, on the
          // scoped thread the parent is joining on, so it is reclaimed
          // exactly once.
          unsafe { HttpObject::from_ptr(ptr) }.read_all_async().await
        })
      })
      .join()
      .unwrap()
    })
  }

  #[instrument(level = "trace", skip_all, fields(self.path), err(Debug))]
  async fn read_all_async(self: Rc<Self>) -> FsResult<Cow<'static, [u8]>> {
    let resp = self.fs.inner.read_response(&self.path, None).await?;
    let resp = ensure_success(resp).await?;
    let data = resp
      .bytes()
      .await
      .map_err(|err| FsError::Io(io::Error::other(err)))?
      .to_vec();

    trace!(nread = data.len());
    Ok(Cow::Owned(data))
  }

  fn chmod_sync(self: Rc<Self>, _pathmode: u32) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  async fn chmod_async(self: Rc<Self>, _mode: u32) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  fn seek_sync(self: Rc<Self>, _pos: io::SeekFrom) -> FsResult<u64> {
    Err(FsError::NotSupported)
  }

  async fn seek_async(self: Rc<Self>, _pos: io::SeekFrom) -> FsResult<u64> {
    Err(FsError::NotSupported)
  }

  fn datasync_sync(self: Rc<Self>) -> FsResult<()> {
    // SAFETY: current thread will be blocked by join()
    let ptr = unsafe { self.into_ptr() };
    std::thread::scope(|s| {
      s.spawn(move || {
        rt::IO_RT.block_on(async move {
          // SAFETY: reconstructs the Rc leaked by into_ptr above, on the
          // scoped thread the parent is joining on, so it is reclaimed
          // exactly once.
          unsafe { HttpObject::from_ptr(ptr) }.datasync_async().await
        })
      })
      .join()
      .unwrap()
    })
  }

  async fn datasync_async(self: Rc<Self>) -> FsResult<()> {
    self.sync_async().await
  }

  fn sync_sync(self: Rc<Self>) -> FsResult<()> {
    // SAFETY: current thread will be blocked by join()
    let ptr = unsafe { self.into_ptr() };
    std::thread::scope(|s| {
      s.spawn(move || {
        rt::IO_RT.block_on(async move {
          // SAFETY: reconstructs the Rc leaked by into_ptr above, on the
          // scoped thread the parent is joining on, so it is reclaimed
          // exactly once.
          unsafe { HttpObject::from_ptr(ptr) }.sync_async().await
        })
      })
      .join()
      .unwrap()
    })
  }

  async fn sync_async(self: Rc<Self>) -> FsResult<()> {
    let mut op_slot = RcRef::map(&self, |r| &r.op_slot).borrow_mut().await;

    if let Some(HttpObjectOpSlot::Write(state)) = op_slot.as_mut()
      && state.dirty
    {
      self
        .fs
        .inner
        .write_object(&self.path, &state.buf, state.overwrite)
        .await?;

      state.dirty = false;
      // The object exists now; a later flush of the same handle is a plain
      // overwrite.
      state.overwrite = true;
    }

    Ok(())
  }

  fn stat_sync(self: Rc<Self>) -> FsResult<FsStat> {
    // SAFETY: current thread will be blocked by join()
    let ptr = unsafe { self.into_ptr() };
    std::thread::scope(|s| {
      s.spawn(move || {
        rt::IO_RT.block_on(async move {
          // SAFETY: reconstructs the Rc leaked by into_ptr above, on the
          // scoped thread the parent is joining on, so it is reclaimed
          // exactly once.
          unsafe { HttpObject::from_ptr(ptr) }.stat_async().await
        })
      })
      .join()
      .unwrap()
    })
  }

  async fn stat_async(self: Rc<Self>) -> FsResult<FsStat> {
    Ok(self.fs.inner.stat_entry(&self.path).await?.into_stat())
  }

  fn lock_sync(self: Rc<Self>, _exclusive: bool) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  async fn lock_async(self: Rc<Self>, _exclusive: bool) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  fn try_lock_sync(self: Rc<Self>, _exclusive: bool) -> FsResult<bool> {
    Err(FsError::NotSupported)
  }

  async fn try_lock_async(self: Rc<Self>, _exclusive: bool) -> FsResult<bool> {
    Err(FsError::NotSupported)
  }

  fn unlock_sync(self: Rc<Self>) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  async fn unlock_async(self: Rc<Self>) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  fn read_at_sync(
    self: Rc<Self>,
    buf: &mut [u8],
    position: u64,
  ) -> FsResult<usize> {
    // SAFETY: current thread will be blocked by join()
    let ptr = unsafe { self.into_ptr() };
    std::thread::scope(|s| {
      s.spawn(move || {
        rt::IO_RT.block_on(async move {
          // SAFETY: reconstructs the Rc leaked by into_ptr above, on the
          // scoped thread the parent is joining on, so it is reclaimed
          // exactly once.
          unsafe { HttpObject::from_ptr(ptr) }
            .read_at_inner(buf, position)
            .await
        })
      })
      .join()
      .unwrap()
    })
  }

  #[instrument(level = "trace", skip_all, fields(self.path, position), err(Debug))]
  async fn read_at_async(
    self: Rc<Self>,
    mut buf: BufMutView,
    position: u64,
  ) -> FsResult<(usize, BufMutView)> {
    let nread = self.read_at_inner(&mut buf, position).await?;
    Ok((nread, buf))
  }

  fn write_at_sync(
    self: Rc<Self>,
    _buf: &[u8],
    _position: u64,
  ) -> FsResult<usize> {
    Err(FsError::NotSupported)
  }

  fn truncate_sync(self: Rc<Self>, _len: u64) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  async fn truncate_async(self: Rc<Self>, _len: u64) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  fn utime_sync(
    self: Rc<Self>,
    _atime_secs: i64,
    _atime_nanos: u32,
    _mtime_secs: i64,
    _mtime_nanos: u32,
  ) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  async fn utime_async(
    self: Rc<Self>,
    _atime_secs: i64,
    _atime_nanos: u32,
    _mtime_secs: i64,
    _mtime_nanos: u32,
  ) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  fn as_stdio(self: Rc<Self>) -> FsResult<std::process::Stdio> {
    Err(FsError::NotSupported)
  }

  fn backing_fd(self: Rc<Self>) -> Option<ResourceHandleFd> {
    None
  }

  fn try_clone_inner(self: Rc<Self>) -> FsResult<Rc<dyn File>> {
    Err(FsError::NotSupported)
  }

  fn maybe_path(&self) -> Option<&Path> {
    None
  }

  fn chown_sync(
    self: Rc<Self>,
    _uid: Option<u32>,
    _gid: Option<u32>,
  ) -> FsResult<()> {
    Err(FsError::NotSupported)
  }

  async fn chown_async(
    self: Rc<Self>,
    _uid: Option<u32>,
    _gid: Option<u32>,
  ) -> FsResult<()> {
    Err(FsError::NotSupported)
  }
}

impl HttpObject {
  /// Positional read via a ranged `GET /read` — independent of the streaming
  /// read state (protocol §5.3 mandates Range support).
  async fn read_at_inner(
    self: &Rc<Self>,
    buf: &mut [u8],
    position: u64,
  ) -> FsResult<usize> {
    if buf.is_empty() {
      return Ok(0);
    }

    let end = position + buf.len() as u64 - 1;
    let resp = self
      .fs
      .inner
      .read_response(&self.path, Some((position, Some(end))))
      .await?;

    // Reading entirely past EOF is not an error, it is EOF.
    if resp.status() == StatusCode::RANGE_NOT_SATISFIABLE {
      return Ok(0);
    }

    let honored_range = resp.status() == StatusCode::PARTIAL_CONTENT;
    let resp = ensure_success(resp).await?;
    let mut reader = Box::pin(
      resp
        .bytes_stream()
        .map_err(io::Error::other)
        .into_async_read(),
    );

    if !honored_range {
      // The server answered with the full body; discard up to `position`.
      let mut remaining = position;
      let mut sink = [0u8; 8192];

      while remaining > 0 {
        let take = remaining.min(sink.len() as u64) as usize;
        let nread = reader.read(&mut sink[..take]).await?;
        if nread == 0 {
          return Ok(0);
        }
        remaining -= nread as u64;
      }
    }

    let mut total = 0;
    while total < buf.len() {
      let nread = reader.read(&mut buf[total..]).await?;
      if nread == 0 {
        break;
      }
      total += nread;
    }

    trace!(nread = total);
    Ok(total)
  }
}

#[cfg(test)]
mod test {
  use std::path::Path;

  use deno_core::serde_json;

  use super::HttpFsConfigs;
  use super::protocol_path;

  #[test]
  fn http_fs_configs_accepts_object_or_array() {
    // Bare config: headers/query default to empty.
    let one: HttpFsConfigs = serde_json::from_value(serde_json::json!({
      "mountPoint": "/objects",
      "baseUrl": "https://api.example.com/fs/v1",
    }))
    .unwrap();
    let mut one = one.into_vec();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].take_mount_point(), "/objects");
    assert!(one[0].headers.is_empty());
    assert!(one[0].query.is_empty());
    assert!(one[0].socket_path.is_none());

    // `socketPath` opts the mount onto a unix socket; `baseUrl` stays an
    // http(s) URL supplying the path prefix / Host / origin.
    let unix: HttpFsConfigs = serde_json::from_value(serde_json::json!({
      "mountPoint": "/objects",
      "baseUrl": "http://localhost/fs/v1",
      "socketPath": "/run/flow/fs.sock",
    }))
    .unwrap();
    let unix = unix.into_vec();
    assert_eq!(unix[0].socket_path.as_deref(), Some("/run/flow/fs.sock"));

    let many: HttpFsConfigs = serde_json::from_value(serde_json::json!([
      {
        "mountPoint": "/a",
        "baseUrl": "https://api.example.com/fs/v1",
        "headers": { "Authorization": "Bearer meow", "X-Api-Key": "woof" },
      },
      {
        "mountPoint": "/b",
        "baseUrl": "https://api.example.com/fs/v1",
        "query": { "token": "meow", "wsId": "42" },
      },
    ]))
    .unwrap();
    let many = many.into_vec();
    assert_eq!(many.len(), 2);
    assert_eq!(many[0].headers.get("Authorization").unwrap(), "Bearer meow");
    assert_eq!(many[0].headers.get("X-Api-Key").unwrap(), "woof");
    assert!(many[0].query.is_empty());
    assert_eq!(many[1].query.get("token").unwrap(), "meow");
    assert_eq!(many[1].query.get("wsId").unwrap(), "42");
    assert!(many[1].headers.is_empty());
  }

  #[test]
  fn protocol_path_normalizes() {
    assert_eq!(protocol_path(Path::new("")).unwrap(), "/");
    assert_eq!(protocol_path(Path::new("/")).unwrap(), "/");
    assert_eq!(protocol_path(Path::new("a/b.txt")).unwrap(), "/a/b.txt");
    assert_eq!(protocol_path(Path::new("/a/b.txt")).unwrap(), "/a/b.txt");
    assert_eq!(protocol_path(Path::new("a//b/./c")).unwrap(), "/a/b/c");
    assert_eq!(protocol_path(Path::new("a/b/../c")).unwrap(), "/a/c");
    assert!(protocol_path(Path::new("../escape")).is_err());
  }
}
