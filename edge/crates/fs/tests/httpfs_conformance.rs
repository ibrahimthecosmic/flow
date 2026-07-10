//! Conformance suite for the HttpFS Protocol v1 client
//! (`fs::http_fs::HttpFs`), run against an in-process mock server. The mock
//! is the reference for what the client expects from a conformant server —
//! see `edge/docs/httpfs-protocol.md` §8.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use deno_core::serde_json;
use deno_core::serde_json::json;
use deno_fs::FileSystem;
use deno_fs::OpenOptions;
use deno_permissions::CheckedPathBuf;
use fs::http_fs::HttpFs;
use fs::http_fs::HttpFsConfig;
use http_body_util::BodyExt;
use http_body_util::Full;
use hyper::Method;
use hyper::Request;
use hyper::Response;
use hyper::StatusCode;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;

const OPEN_READ: OpenOptions = OpenOptions {
  read: true,
  write: false,
  create: false,
  truncate: false,
  append: false,
  create_new: false,
  mode: None,
  custom_flags: None,
};

const OPEN_WRITE: OpenOptions = OpenOptions {
  read: false,
  write: true,
  create: true,
  truncate: true,
  append: false,
  create_new: false,
  mode: None,
  custom_flags: None,
};

const OPEN_APPEND: OpenOptions = OpenOptions {
  read: false,
  write: true,
  create: true,
  truncate: false,
  append: true,
  create_new: false,
  mode: None,
  custom_flags: None,
};

/// Custom headers the client is configured with and must attach to every
/// same-origin request. Names are lowercase — hyper normalizes incoming header
/// names, and `http::HeaderName` sends them lowercased on the wire.
const CRED_HEADERS: &[(&str, &str)] = &[
  ("authorization", "Bearer s3cr3t-t0k3n"),
  ("x-csrf-token", "csrf-v4l"),
];
/// Custom query pairs the client must append to every same-origin request.
const CRED_QUERY: &[(&str, &str)] = &[("wsId", "ws-42"), ("region", "us-1")];

#[derive(Clone, Copy, Debug, PartialEq)]
enum ReadMode {
  Direct,
  /// 307 to `/blob<path>` on the same origin.
  RedirectSameOrigin,
  /// 307 to `http://<other>/blob<path>` (different port = different origin).
  RedirectCrossOrigin,
}

struct State {
  files: HashMap<String, Vec<u8>>,
  dirs: HashSet<String>,
  uploads: HashMap<String, (String, BTreeMap<u64, Vec<u8>>)>,
  upload_seq: u64,

  caps_version: u64,
  direct_write_max_bytes: u64,
  multipart: bool,
  copy: bool,

  read_mode: ReadMode,
  cross_origin_base: Option<String>,
  /// One-shot status returned for the next protocol request.
  fail_next: Option<u16>,

  /// `"<METHOD> <endpoint>"` for every protocol request received.
  log: Vec<String>,
  /// Whether any configured header or query pair accompanied a cross-origin
  /// blob request.
  cross_origin_saw_credential: bool,
}

impl State {
  fn new() -> Self {
    Self {
      files: HashMap::new(),
      dirs: HashSet::from(["/".to_string()]),
      uploads: HashMap::new(),
      upload_seq: 0,
      caps_version: 1,
      direct_write_max_bytes: 0,
      multipart: false,
      copy: true,
      read_mode: ReadMode::Direct,
      cross_origin_base: None,
      fail_next: None,
      log: Vec::new(),
      cross_origin_saw_credential: false,
    }
  }

  fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
      Some(0) => "/",
      Some(idx) => &path[..idx],
      None => "/",
    }
  }

  fn exists(&self, path: &str) -> bool {
    self.files.contains_key(path) || self.dirs.contains(path)
  }

  fn ensure_parents(&mut self, path: &str) {
    let mut current = Self::parent_of(path).to_string();
    while current != "/" {
      self.dirs.insert(current.clone());
      current = Self::parent_of(&current).to_string();
    }
  }

  fn children_of(&self, path: &str) -> Vec<(String, bool)> {
    let prefix = if path == "/" {
      "/".to_string()
    } else {
      format!("{path}/")
    };

    let mut entries = self
      .files
      .keys()
      .filter(|it| it.starts_with(&prefix) && !it[prefix.len()..].contains('/'))
      .map(|it| (it.clone(), true))
      .chain(
        self
          .dirs
          .iter()
          .filter(|it| {
            it.as_str() != "/"
              && it.starts_with(&prefix)
              && !it[prefix.len()..].contains('/')
          })
          .map(|it| (it.clone(), false)),
      )
      .collect::<Vec<_>>();

    entries.sort();
    entries
  }

  fn remove_subtree(&mut self, path: &str) {
    let prefix = format!("{path}/");
    self
      .files
      .retain(|it, _| it != path && !it.starts_with(&prefix));
    self
      .dirs
      .retain(|it| it != path && !it.starts_with(&prefix));
  }
}

type SharedState = Arc<Mutex<State>>;

fn entry_json(path: &str, is_file: bool, size: usize) -> serde_json::Value {
  json!({
    "path": path,
    "kind": if is_file { "file" } else { "dir" },
    "size": size,
    "mtimeMs": 1_730_000_000_000u64,
  })
}

fn json_response(
  status: StatusCode,
  body: serde_json::Value,
) -> Response<Full<Bytes>> {
  Response::builder()
    .status(status)
    .header("content-type", "application/json")
    .body(Full::new(Bytes::from(body.to_string())))
    .unwrap()
}

fn error_response(status: StatusCode, code: &str) -> Response<Full<Bytes>> {
  json_response(
    status,
    json!({ "code": code, "message": format!("mock: {code}") }),
  )
}

fn parse_query(query: Option<&str>) -> HashMap<String, String> {
  url::form_urlencoded::parse(query.unwrap_or_default().as_bytes())
    .into_owned()
    .collect()
}

/// `bytes=a-b` / `bytes=a-` → the requested slice + 206, absent → whole body
/// + 200, unsatisfiable → 416.
fn range_response(data: &[u8], range: Option<&str>) -> Response<Full<Bytes>> {
  let Some(range) = range else {
    return Response::builder()
      .status(StatusCode::OK)
      .body(Full::new(Bytes::from(data.to_vec())))
      .unwrap();
  };

  let spec = range.trim_start_matches("bytes=");
  let (start, end) = spec.split_once('-').unwrap();
  let start = start.parse::<usize>().unwrap();
  let end = end
    .parse::<usize>()
    .map(|it| (it + 1).min(data.len()))
    .unwrap_or(data.len());

  if start >= data.len() {
    return Response::builder()
      .status(StatusCode::RANGE_NOT_SATISFIABLE)
      .body(Full::new(Bytes::new()))
      .unwrap();
  }

  Response::builder()
    .status(StatusCode::PARTIAL_CONTENT)
    .header(
      "content-range",
      format!("bytes {start}-{}/{}", end - 1, data.len()),
    )
    .body(Full::new(Bytes::from(data[start..end].to_vec())))
    .unwrap()
}

/// True only if EVERY configured header and query pair rode along, correct —
/// the client must attach the whole set to every same-origin request.
fn credentials_ok(
  req: &Request<Incoming>,
  params: &HashMap<String, String>,
) -> bool {
  CRED_HEADERS.iter().all(|(name, value)| {
    req
      .headers()
      .get(*name)
      .and_then(|it| it.to_str().ok())
      .is_some_and(|it| it == *value)
  }) && CRED_QUERY
    .iter()
    .all(|(name, value)| params.get(*name).is_some_and(|it| it == *value))
}

/// True if ANY configured header or query key rode along — used to prove a
/// cross-origin target received NONE of them.
fn saw_any_credential(
  req: &Request<Incoming>,
  params: &HashMap<String, String>,
) -> bool {
  CRED_HEADERS
    .iter()
    .any(|(name, _)| req.headers().contains_key(*name))
    || CRED_QUERY
      .iter()
      .any(|(name, _)| params.contains_key(*name))
}

async fn handle(
  state: SharedState,
  origin: String,
  req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
  let method = req.method().clone();
  let uri_path = req.uri().path().to_string();
  let params = parse_query(req.uri().query());

  // The blob route stands in for a storage host: for the cross-origin case
  // it must be reachable WITHOUT the credential; for the same-origin case
  // the client is expected to keep attaching it.
  if let Some(blob_path) = uri_path.strip_prefix("/blob") {
    let mut state = state.lock().unwrap();
    if saw_any_credential(&req, &params) {
      state.cross_origin_saw_credential = true;
    }

    let Some(data) = state.files.get(blob_path).cloned() else {
      return Ok(error_response(StatusCode::NOT_FOUND, "NotFound"));
    };

    let range = req
      .headers()
      .get("range")
      .and_then(|it| it.to_str().ok())
      .map(str::to_owned);
    return Ok(range_response(&data, range.as_deref()));
  }

  // Same-origin blob route: the credential MUST still be attached.
  if let Some(blob_path) = uri_path.strip_prefix("/fs/v1/authed-blob") {
    let state = state.lock().unwrap();
    if !credentials_ok(&req, &params) {
      return Ok(error_response(StatusCode::UNAUTHORIZED, "Unauthenticated"));
    }

    let Some(data) = state.files.get(blob_path).cloned() else {
      return Ok(error_response(StatusCode::NOT_FOUND, "NotFound"));
    };

    let range = req
      .headers()
      .get("range")
      .and_then(|it| it.to_str().ok())
      .map(str::to_owned);
    return Ok(range_response(&data, range.as_deref()));
  }

  let Some(endpoint) = uri_path.strip_prefix("/fs/v1") else {
    return Ok(error_response(StatusCode::NOT_FOUND, "NotFound"));
  };
  let endpoint = endpoint.to_string();

  {
    let mut state = state.lock().unwrap();
    state.log.push(format!("{method} {endpoint}"));

    if let Some(status) = state.fail_next.take() {
      return Ok(error_response(
        StatusCode::from_u16(status).unwrap(),
        "Internal",
      ));
    }

    if !credentials_ok(&req, &params) {
      return Ok(error_response(StatusCode::UNAUTHORIZED, "Unauthenticated"));
    }
  }

  let range = req
    .headers()
    .get("range")
    .and_then(|it| it.to_str().ok())
    .map(str::to_owned);
  let body = req.into_body().collect().await?.to_bytes();

  let mut state = state.lock().unwrap();
  let resp = route(
    &mut state, &origin, &method, &endpoint, &params, range, body,
  );
  Ok(resp)
}

fn route(
  state: &mut State,
  origin: &str,
  method: &Method,
  endpoint: &str,
  params: &HashMap<String, String>,
  range: Option<String>,
  body: Bytes,
) -> Response<Full<Bytes>> {
  let path = || params.get("path").cloned().unwrap_or_default();

  match (method, endpoint) {
    (&Method::GET, "/capabilities") => {
      let mut caps = json!({
        "version": state.caps_version,
        "directWriteMaxBytes": state.direct_write_max_bytes,
        "readRedirect": state.read_mode != ReadMode::Direct,
        "copy": state.copy,
      });
      if state.multipart {
        caps["multipart"] = json!({
          "minPartBytes": 4,
          "maxPartBytes": 1_073_741_824u64,
          "maxParts": 1000,
        });
      }
      json_response(StatusCode::OK, caps)
    }

    (&Method::GET, "/stat") => {
      let path = path();
      if let Some(data) = state.files.get(&path) {
        json_response(StatusCode::OK, entry_json(&path, true, data.len()))
      } else if state.dirs.contains(&path) {
        json_response(StatusCode::OK, entry_json(&path, false, 0))
      } else {
        error_response(StatusCode::NOT_FOUND, "NotFound")
      }
    }

    (&Method::GET, "/list") => {
      let path = path();
      if state.files.contains_key(&path) {
        return error_response(StatusCode::CONFLICT, "NotADirectory");
      }
      if !state.dirs.contains(&path) {
        return error_response(StatusCode::NOT_FOUND, "NotFound");
      }

      // Two entries per page, cursor = next start index, to exercise the
      // client's pagination loop.
      let children = state.children_of(&path);
      let start = params
        .get("cursor")
        .and_then(|it| it.parse::<usize>().ok())
        .unwrap_or(0);
      let page = children.iter().skip(start).take(2).collect::<Vec<_>>();
      let next = start + page.len();

      json_response(
        StatusCode::OK,
        json!({
          "entries": page
            .iter()
            .map(|(entry_path, is_file)| entry_json(
              entry_path,
              *is_file,
              state.files.get(entry_path).map(Vec::len).unwrap_or(0),
            ))
            .collect::<Vec<_>>(),
          "cursor": if next < children.len() {
            json!(next.to_string())
          } else {
            json!(null)
          },
        }),
      )
    }

    (&Method::GET, "/read") => {
      let path = path();
      if state.dirs.contains(&path) {
        return error_response(StatusCode::CONFLICT, "IsADirectory");
      }
      let Some(data) = state.files.get(&path).cloned() else {
        return error_response(StatusCode::NOT_FOUND, "NotFound");
      };

      match state.read_mode {
        ReadMode::Direct => range_response(&data, range.as_deref()),
        ReadMode::RedirectSameOrigin => Response::builder()
          .status(StatusCode::TEMPORARY_REDIRECT)
          .header("location", format!("{origin}/fs/v1/authed-blob{path}"))
          .body(Full::new(Bytes::new()))
          .unwrap(),
        ReadMode::RedirectCrossOrigin => Response::builder()
          .status(StatusCode::TEMPORARY_REDIRECT)
          .header(
            "location",
            format!(
              "{}/blob{path}",
              state.cross_origin_base.as_deref().unwrap()
            ),
          )
          .body(Full::new(Bytes::new()))
          .unwrap(),
      }
    }

    (&Method::PUT, "/write") => {
      let path = path();
      let overwrite =
        params.get("overwrite").map(String::as_str) != Some("false");

      if state.dirs.contains(&path) {
        return error_response(StatusCode::CONFLICT, "IsADirectory");
      }
      if !overwrite && state.files.contains_key(&path) {
        return error_response(StatusCode::CONFLICT, "AlreadyExists");
      }
      if state.direct_write_max_bytes > 0
        && body.len() as u64 > state.direct_write_max_bytes
      {
        return error_response(StatusCode::PAYLOAD_TOO_LARGE, "TooLarge");
      }

      state.ensure_parents(&path);
      let len = body.len();
      state.files.insert(path.clone(), body.to_vec());
      json_response(StatusCode::OK, entry_json(&path, true, len))
    }

    (&Method::POST, "/mkdir") => {
      let path = path();
      let parents = params.get("parents").map(String::as_str) == Some("true");

      if state.files.contains_key(&path) {
        return error_response(StatusCode::CONFLICT, "NotADirectory");
      }
      if state.dirs.contains(&path) {
        if parents {
          return json_response(StatusCode::OK, entry_json(&path, false, 0));
        }
        return error_response(StatusCode::CONFLICT, "AlreadyExists");
      }
      if !parents && !state.dirs.contains(State::parent_of(&path)) {
        return error_response(StatusCode::NOT_FOUND, "NotFound");
      }

      state.ensure_parents(&path);
      state.dirs.insert(path.clone());
      json_response(StatusCode::OK, entry_json(&path, false, 0))
    }

    (&Method::DELETE, "/remove") => {
      let path = path();
      let recursive =
        params.get("recursive").map(String::as_str) == Some("true");

      if state.files.remove(&path).is_some() {
        return Response::builder()
          .status(StatusCode::NO_CONTENT)
          .body(Full::new(Bytes::new()))
          .unwrap();
      }
      if !state.dirs.contains(&path) {
        return error_response(StatusCode::NOT_FOUND, "NotFound");
      }
      if !recursive && !state.children_of(&path).is_empty() {
        return error_response(StatusCode::CONFLICT, "NotEmpty");
      }

      state.remove_subtree(&path);
      Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Full::new(Bytes::new()))
        .unwrap()
    }

    (&Method::POST, "/move") | (&Method::POST, "/copy") => {
      let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
      let from = body["from"].as_str().unwrap().to_string();
      let to = body["to"].as_str().unwrap().to_string();
      let overwrite = body["overwrite"].as_bool().unwrap_or(false);
      let is_move = endpoint == "/move";

      if !state.exists(&from) {
        return error_response(StatusCode::NOT_FOUND, "NotFound");
      }
      if !overwrite && state.exists(&to) {
        return error_response(StatusCode::CONFLICT, "AlreadyExists");
      }

      if let Some(data) = state.files.get(&from).cloned() {
        let len = data.len();
        state.ensure_parents(&to);
        state.files.insert(to.clone(), data);
        if is_move {
          state.files.remove(&from);
        }
        json_response(StatusCode::OK, entry_json(&to, true, len))
      } else if is_move {
        // Move a directory subtree.
        let from_prefix = format!("{from}/");
        let moved_files = state
          .files
          .iter()
          .filter(|(it, _)| it.starts_with(&from_prefix))
          .map(|(it, data)| {
            (format!("{to}{}", &it[from.len()..]), data.clone())
          })
          .collect::<Vec<_>>();
        let moved_dirs = state
          .dirs
          .iter()
          .filter(|it| it.starts_with(&from_prefix))
          .map(|it| format!("{to}{}", &it[from.len()..]))
          .collect::<Vec<_>>();

        state.remove_subtree(&from);
        state.ensure_parents(&to);
        state.dirs.insert(to.clone());
        state.files.extend(moved_files);
        state.dirs.extend(moved_dirs);
        json_response(StatusCode::OK, entry_json(&to, false, 0))
      } else {
        // The mock does not implement recursive server-side copy; the
        // client is expected to walk directories itself.
        error_response(StatusCode::CONFLICT, "IsADirectory")
      }
    }

    (&Method::POST, "/upload") => {
      state.upload_seq += 1;
      let upload_id = format!("up-{}", state.upload_seq);
      state
        .uploads
        .insert(upload_id.clone(), (path(), BTreeMap::new()));
      json_response(StatusCode::OK, json!({ "uploadId": upload_id }))
    }

    (&Method::POST, "/upload/part") => {
      let upload_id = params.get("uploadId").cloned().unwrap_or_default();
      let part_number = params.get("partNumber").cloned().unwrap_or_default();
      if !state.uploads.contains_key(&upload_id) {
        return error_response(StatusCode::NOT_FOUND, "NotFound");
      }

      json_response(
        StatusCode::OK,
        json!({
          "url": format!(
            "{origin}/fs/v1/upload/put?uploadId={upload_id}&partNumber={part_number}"
          ),
          "expiresAtMs": 1_900_000_000_000u64,
        }),
      )
    }

    (&Method::PUT, "/upload/put") => {
      let upload_id = params.get("uploadId").cloned().unwrap_or_default();
      let part_number = params
        .get("partNumber")
        .and_then(|it| it.parse::<u64>().ok())
        .unwrap_or_default();
      let Some((_, parts)) = state.uploads.get_mut(&upload_id) else {
        return error_response(StatusCode::NOT_FOUND, "NotFound");
      };

      parts.insert(part_number, body.to_vec());
      Response::builder()
        .status(StatusCode::OK)
        .header("etag", format!("\"etag-{part_number}\""))
        .body(Full::new(Bytes::new()))
        .unwrap()
    }

    (&Method::POST, "/upload/commit") => {
      let upload_id = params.get("uploadId").cloned().unwrap_or_default();
      let Some((path, parts)) = state.uploads.remove(&upload_id) else {
        return error_response(StatusCode::NOT_FOUND, "NotFound");
      };

      let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
      assert_eq!(
        body["parts"].as_array().unwrap().len(),
        parts.len(),
        "commit must list every uploaded part"
      );

      let data = parts.into_values().flatten().collect::<Vec<_>>();
      let len = data.len();
      state.ensure_parents(&path);
      state.files.insert(path.clone(), data);
      json_response(StatusCode::OK, entry_json(&path, true, len))
    }

    (&Method::DELETE, "/upload") => {
      let upload_id = params.get("uploadId").cloned().unwrap_or_default();
      state.uploads.remove(&upload_id);
      Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Full::new(Bytes::new()))
        .unwrap()
    }

    _ => error_response(StatusCode::NOT_FOUND, "NotFound"),
  }
}

async fn spawn_server(state: SharedState) -> SocketAddr {
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let addr = listener.local_addr().unwrap();
  let origin = format!("http://{addr}");

  tokio::spawn(async move {
    loop {
      let Ok((stream, _)) = listener.accept().await else {
        break;
      };
      let state = state.clone();
      let origin = origin.clone();

      tokio::spawn(async move {
        let _ = hyper::server::conn::http1::Builder::new()
          .serve_connection(
            TokioIo::new(stream),
            service_fn(move |req| handle(state.clone(), origin.clone(), req)),
          )
          .await;
      });
    }
  });

  addr
}

/// Serves the same mock over an AF_UNIX socket. The generated redirect /
/// part URLs use `http://localhost` so the client — whose `baseUrl` host is
/// `localhost` — treats them as same-origin and keeps them on the socket.
fn spawn_unix_server(state: SharedState, socket: PathBuf) {
  let listener = tokio::net::UnixListener::bind(&socket).unwrap();

  tokio::spawn(async move {
    loop {
      let Ok((stream, _)) = listener.accept().await else {
        break;
      };
      let state = state.clone();

      tokio::spawn(async move {
        let _ = hyper::server::conn::http1::Builder::new()
          .serve_connection(
            TokioIo::new(stream),
            service_fn(move |req| {
              handle(state.clone(), "http://localhost".to_string(), req)
            }),
          )
          .await;
      });
    }
  });
}

fn http_fs_config(addr: SocketAddr) -> HttpFsConfig {
  let headers: serde_json::Map<String, serde_json::Value> = CRED_HEADERS
    .iter()
    .map(|(name, value)| (name.to_string(), json!(value)))
    .collect();
  let query: serde_json::Map<String, serde_json::Value> = CRED_QUERY
    .iter()
    .map(|(name, value)| (name.to_string(), json!(value)))
    .collect();
  serde_json::from_value(json!({
    "mountPoint": "/objects",
    "baseUrl": format!("http://{addr}/fs/v1"),
    "headers": headers,
    "query": query,
  }))
  .unwrap()
}

async fn setup(state: State) -> (HttpFs, SharedState, SocketAddr) {
  let state = Arc::new(Mutex::new(state));
  let addr = spawn_server(state.clone()).await;
  let fs = HttpFs::new(http_fs_config(addr)).unwrap();

  (fs, state, addr)
}

fn http_fs_config_unix(socket: &std::path::Path) -> HttpFsConfig {
  let headers: serde_json::Map<String, serde_json::Value> = CRED_HEADERS
    .iter()
    .map(|(name, value)| (name.to_string(), json!(value)))
    .collect();
  let query: serde_json::Map<String, serde_json::Value> = CRED_QUERY
    .iter()
    .map(|(name, value)| (name.to_string(), json!(value)))
    .collect();
  serde_json::from_value(json!({
    "mountPoint": "/objects",
    "baseUrl": "http://localhost/fs/v1",
    "socketPath": socket.to_str().unwrap(),
    "headers": headers,
    "query": query,
  }))
  .unwrap()
}

fn setup_unix(state: State) -> (HttpFs, SharedState, tempfile::TempDir) {
  let state = Arc::new(Mutex::new(state));
  let dir = tempfile::tempdir().unwrap();
  let socket = dir.path().join("fs.sock");
  spawn_unix_server(state.clone(), socket.clone());
  let fs = HttpFs::new(http_fs_config_unix(&socket)).unwrap();

  (fs, state, dir)
}

fn checked(path: &str) -> CheckedPathBuf {
  CheckedPathBuf::unsafe_new(PathBuf::from(path))
}

async fn write(fs: &HttpFs, path: &str, data: &[u8]) {
  fs.write_file_async(checked(path), OPEN_WRITE, data.to_vec().into())
    .await
    .unwrap();
}

async fn read(fs: &HttpFs, path: &str) -> Vec<u8> {
  fs.read_file_async(checked(path), OPEN_READ)
    .await
    .unwrap()
    .into_owned()
}

#[tokio::test]
async fn roundtrip_write_stat_read_list_remove() {
  let (fs, _state, _) = setup(State::new()).await;

  fs.mkdir_async(checked("reports"), true, None)
    .await
    .unwrap();
  write(&fs, "reports/q1.txt", b"first quarter").await;

  let stat = fs.stat_async(checked("reports/q1.txt")).await.unwrap();
  assert!(stat.is_file);
  assert_eq!(stat.size, 13);

  let stat = fs.stat_async(checked("reports")).await.unwrap();
  assert!(stat.is_directory);

  assert_eq!(read(&fs, "reports/q1.txt").await, b"first quarter");

  let rd = fs.read_dir_async(checked("reports")).await.unwrap();
  let mut names = Vec::new();
  while let Some(entry) = deno_fs::FsReadDir::next(&*rd).await.unwrap() {
    names.push((entry.name, entry.is_file));
  }
  assert_eq!(names, vec![("q1.txt".to_string(), true)]);

  fs.remove_async(checked("reports/q1.txt"), false)
    .await
    .unwrap();
  let err = fs
    .stat_async(checked("reports/q1.txt"))
    .await
    .err()
    .unwrap();
  assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

#[tokio::test]
async fn custom_headers_and_query_attached() {
  // Every protocol request 401s unless ALL configured headers AND query pairs
  // are present (`credentials_ok`), so a successful roundtrip proves the whole
  // set — multiple headers and multiple query pairs — attaches together.
  let (fs, _state, _) = setup(State::new()).await;

  write(&fs, "a.txt", b"data").await;
  assert_eq!(read(&fs, "a.txt").await, b"data");
}

#[tokio::test]
async fn capabilities_version_gate() {
  let mut state = State::new();
  state.caps_version = 2;
  let (fs, _state, _) = setup(state).await;

  let err = fs
    .write_file_async(checked("a.txt"), OPEN_WRITE, b"data".to_vec().into())
    .await
    .unwrap_err();
  assert!(
    err.to_string().contains("protocol version"),
    "unexpected error: {err}"
  );
}

#[tokio::test]
async fn errno_mappings() {
  let (fs, _state, _) = setup(State::new()).await;

  // ENOENT
  let err = fs.stat_async(checked("missing")).await.err().unwrap();
  assert_eq!(err.kind(), io::ErrorKind::NotFound);

  // EEXIST — mkdir of an existing dir without parents
  fs.mkdir_async(checked("dir"), false, None).await.unwrap();
  let err = fs
    .mkdir_async(checked("dir"), false, None)
    .await
    .unwrap_err();
  assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);

  // ENOTEMPTY — non-recursive remove of a non-empty dir
  write(&fs, "dir/file.txt", b"x").await;
  let err = fs.remove_async(checked("dir"), false).await.unwrap_err();
  assert_eq!(err.kind(), io::ErrorKind::DirectoryNotEmpty);

  // ENOTDIR — list on a file
  let err = fs
    .read_dir_async(checked("dir/file.txt"))
    .await
    .unwrap_err();
  assert_eq!(err.kind(), io::ErrorKind::NotADirectory);

  // EISDIR — open a dir as a file
  let err = fs
    .open_async(checked("dir"), OPEN_READ)
    .await
    .err()
    .unwrap();
  assert_eq!(err.kind(), io::ErrorKind::IsADirectory);

  // EEXIST — O_EXCL on an existing file
  let err = fs
    .open_async(
      checked("dir/file.txt"),
      OpenOptions {
        create_new: true,
        create: true,
        write: true,
        ..OPEN_WRITE
      },
    )
    .await
    .err()
    .unwrap();
  assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
}

#[tokio::test]
async fn read_redirect_cross_origin_strips_credential() {
  // A second origin (different port) hosting the blobs.
  let blob_state = Arc::new(Mutex::new(State::new()));
  let blob_addr = spawn_server(blob_state.clone()).await;

  let mut state = State::new();
  state.read_mode = ReadMode::RedirectCrossOrigin;
  state.cross_origin_base = Some(format!("http://{blob_addr}"));
  let (fs, state, _) = setup(state).await;

  write(&fs, "a.txt", b"redirected bytes").await;
  fs.flush_background_tasks().await;
  blob_state.lock().unwrap().files = state.lock().unwrap().files.clone();

  assert_eq!(read(&fs, "a.txt").await, b"redirected bytes");
  assert!(
    !blob_state.lock().unwrap().cross_origin_saw_credential,
    "credential must be stripped on cross-origin redirects"
  );
}

#[tokio::test]
async fn read_redirect_same_origin_keeps_credential() {
  let mut state = State::new();
  state.read_mode = ReadMode::RedirectSameOrigin;
  let (fs, _state, _) = setup(state).await;

  write(&fs, "a.txt", b"redirected bytes").await;

  // The same-origin blob route 401s without the credential, so a successful
  // read proves it was re-attached.
  assert_eq!(read(&fs, "a.txt").await, b"redirected bytes");
}

#[tokio::test]
async fn copy_uses_capability_when_declared() {
  let (fs, state, _) = setup(State::new()).await;

  write(&fs, "src.txt", b"copy me").await;
  fs.copy_file_async(checked("src.txt"), checked("dst.txt"))
    .await
    .unwrap();

  assert_eq!(read(&fs, "dst.txt").await, b"copy me");
  assert!(
    state
      .lock()
      .unwrap()
      .log
      .iter()
      .any(|it| it == "POST /copy"),
    "client should use POST /copy when the capability is declared"
  );
}

#[tokio::test]
async fn copy_falls_back_to_read_write_without_capability() {
  let mut state = State::new();
  state.copy = false;
  let (fs, state, _) = setup(state).await;

  write(&fs, "src.txt", b"copy me").await;
  fs.copy_file_async(checked("src.txt"), checked("dst.txt"))
    .await
    .unwrap();

  assert_eq!(read(&fs, "dst.txt").await, b"copy me");
  assert!(
    !state
      .lock()
      .unwrap()
      .log
      .iter()
      .any(|it| it == "POST /copy"),
    "client must not call POST /copy when the capability is absent"
  );
}

#[tokio::test]
async fn cp_copies_directories_recursively() {
  let (fs, _state, _) = setup(State::new()).await;

  write(&fs, "tree/a.txt", b"a").await;
  write(&fs, "tree/sub/b.txt", b"b").await;

  fs.cp_async(checked("tree"), checked("tree2"))
    .await
    .unwrap();

  assert_eq!(read(&fs, "tree2/a.txt").await, b"a");
  assert_eq!(read(&fs, "tree2/sub/b.txt").await, b"b");
}

#[tokio::test]
async fn rename_moves_files_and_directories() {
  let (fs, _state, _) = setup(State::new()).await;

  write(&fs, "old.txt", b"payload").await;
  fs.rename_async(checked("old.txt"), checked("new.txt"))
    .await
    .unwrap();

  let err = fs.stat_async(checked("old.txt")).await.err().unwrap();
  assert_eq!(err.kind(), io::ErrorKind::NotFound);
  assert_eq!(read(&fs, "new.txt").await, b"payload");

  write(&fs, "dir/inner.txt", b"inner").await;
  fs.rename_async(checked("dir"), checked("dir2"))
    .await
    .unwrap();
  assert_eq!(read(&fs, "dir2/inner.txt").await, b"inner");
}

#[tokio::test]
async fn list_paginates_with_cursor() {
  let (fs, state, _) = setup(State::new()).await;

  for idx in 0..5 {
    write(&fs, &format!("many/f{idx}.txt"), b"x").await;
  }

  let rd = fs.read_dir_async(checked("many")).await.unwrap();
  let mut count = 0;
  while deno_fs::FsReadDir::next(&*rd).await.unwrap().is_some() {
    count += 1;
  }
  assert_eq!(count, 5);

  // 5 entries at 2 per page = 3 list requests.
  assert_eq!(
    state
      .lock()
      .unwrap()
      .log
      .iter()
      .filter(|it| *it == "GET /list")
      .count(),
    3
  );
}

#[tokio::test]
async fn ranged_read_at() {
  let (fs, _state, _) = setup(State::new()).await;

  write(&fs, "a.txt", b"0123456789").await;

  let file = fs.open_async(checked("a.txt"), OPEN_READ).await.unwrap();
  let buf = deno_core::BufMutView::from(bytes::BytesMut::zeroed(4));
  let (nread, buf) = file.clone().read_at_async(buf, 3).await.unwrap();
  assert_eq!(nread, 4);
  assert_eq!(&buf[..nread], b"3456");

  // Entirely past EOF → EOF, not an error.
  let buf = deno_core::BufMutView::from(bytes::BytesMut::zeroed(4));
  let (nread, _) = file.read_at_async(buf, 100).await.unwrap();
  assert_eq!(nread, 0);
}

#[tokio::test]
async fn multipart_upload_over_direct_write_limit() {
  let mut state = State::new();
  state.direct_write_max_bytes = 8;
  state.multipart = true;
  let (fs, state, _) = setup(state).await;

  let data = b"twenty bytes of data".to_vec();
  assert_eq!(data.len(), 20);
  write(&fs, "big.bin", &data).await;

  assert_eq!(read(&fs, "big.bin").await, data);
  {
    let state = state.lock().unwrap();
    assert!(state.log.iter().any(|it| it == "POST /upload/commit"));
    assert!(
      !state.log.iter().any(|it| it == "PUT /write"),
      "a body over directWriteMaxBytes must not go through PUT /write"
    );
  }
}

#[tokio::test]
async fn oversized_write_without_multipart_is_efbig() {
  let mut state = State::new();
  state.direct_write_max_bytes = 8;
  let (fs, _state, _) = setup(state).await;

  let err = fs
    .write_file_async(
      checked("big.bin"),
      OPEN_WRITE,
      b"twenty bytes of data".to_vec().into(),
    )
    .await
    .unwrap_err();
  assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
}

#[tokio::test]
async fn idempotent_requests_retry_on_500() {
  let (fs, state, _) = setup(State::new()).await;

  write(&fs, "a.txt", b"data").await;
  fs.flush_background_tasks().await;

  state.lock().unwrap().fail_next = Some(500);
  let stat = fs.stat_async(checked("a.txt")).await.unwrap();
  assert!(stat.is_file);
}

#[tokio::test]
async fn append_rewrites_with_existing_content() {
  let (fs, _state, _) = setup(State::new()).await;

  write(&fs, "log.txt", b"hello").await;

  fs.write_file_async(
    checked("log.txt"),
    OPEN_APPEND,
    b" world".to_vec().into(),
  )
  .await
  .unwrap();

  assert_eq!(read(&fs, "log.txt").await, b"hello world");
}

#[tokio::test]
async fn truncate_shrinks_and_extends() {
  let (fs, _state, _) = setup(State::new()).await;

  write(&fs, "a.txt", b"0123456789").await;

  fs.truncate_async(checked("a.txt"), 4).await.unwrap();
  assert_eq!(read(&fs, "a.txt").await, b"0123");

  fs.truncate_async(checked("a.txt"), 6).await.unwrap();
  assert_eq!(read(&fs, "a.txt").await, b"0123\0\0");

  fs.truncate_async(checked("a.txt"), 0).await.unwrap();
  assert_eq!(read(&fs, "a.txt").await, b"");
}

#[tokio::test]
async fn realpath_and_exists() {
  let (fs, _state, _) = setup(State::new()).await;

  write(&fs, "dir/a.txt", b"x").await;

  assert_eq!(
    fs.realpath_async(checked("dir/a.txt")).await.unwrap(),
    PathBuf::from("/dir/a.txt")
  );
  let err = fs.realpath_async(checked("missing")).await.unwrap_err();
  assert_eq!(err.kind(), io::ErrorKind::NotFound);

  assert!(fs.exists_async(checked("dir/a.txt")).await.unwrap());
  assert!(!fs.exists_async(checked("missing")).await.unwrap());
}

// The sync surface shares *_inner with the async one; this smoke test guards
// the scoped-thread + IO_RT plumbing. multi_thread is required: the sync
// call parks the calling thread while the mock server must keep serving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_surface_works() {
  let (fs, _state, _) = setup(State::new()).await;

  let fs_clone = fs.clone();
  tokio::task::spawn_blocking(move || {
    let path = checked("sync.txt");
    fs_clone
      .write_file_sync(&path.as_checked_path(), OPEN_WRITE, b"sync bytes")
      .unwrap();
    let data = fs_clone
      .read_file_sync(&path.as_checked_path(), OPEN_READ)
      .unwrap();
    assert_eq!(&*data, b"sync bytes");
    assert!(fs_clone.stat_sync(&path.as_checked_path()).unwrap().is_file);
  })
  .await
  .unwrap();
}

// The unix-socket transport swaps only how bytes reach the server; the whole
// protocol layer is shared with the TCP path. These tests prove the shim
// end-to-end, including same-origin redirect routing over the socket and the
// sync surface.

#[tokio::test]
async fn unix_socket_roundtrip() {
  let (fs, _state, _dir) = setup_unix(State::new());

  fs.mkdir_async(checked("reports"), true, None)
    .await
    .unwrap();
  write(&fs, "reports/q1.txt", b"over a socket").await;

  let stat = fs.stat_async(checked("reports/q1.txt")).await.unwrap();
  assert!(stat.is_file);
  assert_eq!(stat.size, 13);

  assert_eq!(read(&fs, "reports/q1.txt").await, b"over a socket");

  let rd = fs.read_dir_async(checked("reports")).await.unwrap();
  let mut names = Vec::new();
  while let Some(entry) = deno_fs::FsReadDir::next(&*rd).await.unwrap() {
    names.push((entry.name, entry.is_file));
  }
  assert_eq!(names, vec![("q1.txt".to_string(), true)]);

  fs.remove_async(checked("reports/q1.txt"), false)
    .await
    .unwrap();
  let err = fs
    .stat_async(checked("reports/q1.txt"))
    .await
    .err()
    .unwrap();
  assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

#[tokio::test]
async fn unix_socket_read_redirect_same_origin() {
  let mut state = State::new();
  state.read_mode = ReadMode::RedirectSameOrigin;
  let (fs, _state, _dir) = setup_unix(state);

  write(&fs, "a.txt", b"redirected over socket").await;

  // The redirect target (`http://localhost/...authed-blob`) is same-origin, so
  // it must stay on the socket AND keep the credential — the authed-blob route
  // 401s without it.
  assert_eq!(read(&fs, "a.txt").await, b"redirected over socket");
}

#[tokio::test]
async fn unix_socket_multipart_upload() {
  let mut state = State::new();
  state.direct_write_max_bytes = 8;
  state.multipart = true;
  let (fs, state, _dir) = setup_unix(state);

  let data = b"twenty bytes of data".to_vec();
  write(&fs, "big.bin", &data).await;

  assert_eq!(read(&fs, "big.bin").await, data);
  let state = state.lock().unwrap();
  assert!(state.log.iter().any(|it| it == "POST /upload/commit"));
  assert!(!state.log.iter().any(|it| it == "PUT /write"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unix_socket_sync_surface() {
  let (fs, _state, _dir) = setup_unix(State::new());

  let fs_clone = fs.clone();
  tokio::task::spawn_blocking(move || {
    let path = checked("sync.txt");
    fs_clone
      .write_file_sync(&path.as_checked_path(), OPEN_WRITE, b"sync bytes")
      .unwrap();
    let data = fs_clone
      .read_file_sync(&path.as_checked_path(), OPEN_READ)
      .unwrap();
    assert_eq!(&*data, b"sync bytes");
  })
  .await
  .unwrap();
}
