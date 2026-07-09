# HttpFS Protocol v1 (draft)

Status: **draft** — not yet frozen. Breaking changes allowed until the first
HttpFS release ships.

HttpFS is a virtual filesystem mounted into flow user workers (e.g. at
`/objects`). It is backed by any HTTP API that implements this protocol. The
runtime's Rust client (`edge/crates/fs/impl/http_fs.rs`) is the sole consumer;
conformance is defined by the test suite in
`edge/crates/fs/tests/httpfs_conformance.rs`, not by any particular server
implementation.

Design goals, in priority order:

1. **Path-native** — every operation takes a filesystem path. The server owns
   path→storage resolution; the client never sees or manages backend ids.
2. **One round trip per fs op** in the common case. Sync fs calls
   (`readFileSync`) block a worker thread on the network; chatty flows are
   unacceptable.
3. **Backend-agnostic** — implementable over S3-backed stores, databases, plain
   disk, or anything else. Presigned-URL indirection and multipart upload are
   optional capabilities, not requirements.
4. **Auth-agnostic** — the runtime attaches caller-configured headers and query
   params to every request; it has no notion of authentication. Account model,
   roles, scoping, and revocation are entirely the server's concern.

## 1. Transport

- HTTPS (HTTP permitted for local development), over TCP or an AF_UNIX socket.
- All endpoints are relative to a configured `baseUrl`, which may include any
  path prefix (e.g. `https://api.example.com/fs/v1`). The protocol does not
  reserve a version segment; versioning is negotiated via `GET /capabilities`.
- Request and response bodies are JSON (`application/json`) unless stated
  otherwise (raw file bytes on read/write).
- The client sends `accept-encoding` as usual; servers may compress JSON
  responses. File bytes are transferred verbatim.

### 1.1 Custom headers & query

The runtime is configured with two optional maps of caller-supplied `headers`
and `query` params — anything auth needs (bearer token, API key, CSRF token,
workspace id) lives here:

```jsonc
// worker create option (see §7)
"headers": { "Authorization": "Bearer <token>", "X-CSRF-Token": "<t>" },
"query":   { "wsId": "<id>" }
// → every request carries those headers and ?wsId=<id>
```

The client attaches both maps to **every** request, including redirects only
when the redirect target has the same origin as `baseUrl` (a cross-origin
redirect target, e.g. a presigned URL, is fetched with neither). Custom `query`
keys are appended alongside protocol params — avoid the reserved names in §5
(`path`, `cursor`, `uploadId`, `partNumber`, `contentType`, `overwrite`,
`parents`, `recursive`). Servers respond `401` for missing/invalid credentials
and `403` for valid credentials lacking permission for the specific operation.

### 1.2 Unix-socket transport

For a server that shares the worker's host (a sidecar), the mount may be
configured with a `socketPath` (§7): protocol requests are then made over that
AF_UNIX socket instead of TCP. `baseUrl` stays an http(s) URL, but its host is a
placeholder — it supplies only the path prefix, the `Host` header, and the
origin used to scope credentials across redirects (use e.g.
`http://localhost/fs/v1`). The wire protocol is otherwise identical; a
conformant server needs no changes to be reachable over a socket.

A cross-origin redirect or presigned-upload target (§5.3/§5.5) names a real host
and is always fetched over TCP, even for a unix-socket mount — so a local API
can still hand off large transfers to object storage. The client opens a fresh
socket connection per request (no pooling) and buffers each response body in
full rather than streaming it.

## 2. Paths

- UTF-8, `/`-separated, always absolute from the mount root: `/reports/q1.pdf`
  means `<mountPoint>/reports/q1.pdf` inside the worker.
- The client normalizes before sending: no `.` or `..` segments, no duplicate or
  trailing slashes (except the root `/`). Servers MUST reject non-normalized
  paths with `400` (defense in depth).
- Path segments MUST NOT contain `/` or NUL. Servers MAY impose further
  restrictions (max length, character set) and reject with `400`; the error
  message should say why.
- Paths are passed in the `path` query parameter, percent-encoded.
- The server maps the path root to whatever the request's credentials are scoped
  to (e.g. a workspace). Different credentials may see entirely different trees.

## 3. Common types

### 3.1 Entry

```jsonc
{
  "path": "/reports/q1.pdf", // normalized absolute path
  "kind": "file", // "file" | "dir"
  "size": 1048576, // bytes; 0 for dirs
  "mtimeMs": 1730000000000, // last modified, epoch milliseconds
  "birthtimeMs": 1720000000000, // optional; created time
  "contentType": "application/pdf", // optional; files only
  "etag": "abc123" // optional; changes when content changes
}
```

`mtimeMs` is required. When the backend has no real mtime, return the best
available proxy (e.g. row `updatedAt`). All other optional fields may be
omitted; the client fakes fs metadata it can't get (mode/uid/gid), matching
existing S3Fs behavior.

### 3.2 Errors

Non-2xx responses carry:

```jsonc
{ "code": "NotFound", "message": "no such file: /reports/q1.pdf" }
```

`code` values and their fs mapping (client-side errno):

| code               | HTTP | errno       | meaning                                     |
| ------------------ | ---- | ----------- | ------------------------------------------- |
| `NotFound`         | 404  | `ENOENT`    | path does not exist                         |
| `AlreadyExists`    | 409  | `EEXIST`    | destination already exists                  |
| `NotADirectory`    | 409  | `ENOTDIR`   | path component is a file                    |
| `IsADirectory`     | 409  | `EISDIR`    | file op on a dir                            |
| `NotEmpty`         | 409  | `ENOTEMPTY` | non-recursive delete of a non-empty dir     |
| `PermissionDenied` | 403  | `EPERM`     | credentials lack permission                 |
| `Unauthenticated`  | 401  | `EPERM`     | missing/invalid credentials                 |
| `TooLarge`         | 413  | `EFBIG`     | body exceeds declared write limit           |
| `InvalidPath`      | 400  | `EINVAL`    | malformed path or request                   |
| `RateLimited`      | 429  | —           | client retries with backoff (see §6)        |
| `Internal`         | 5xx  | `EIO`       | server fault; client retries idempotent ops |

Unknown `code` values map to `EIO`. Servers SHOULD use the listed HTTP statuses;
the client keys on `code` first, status second.

## 4. Capabilities

```
GET /capabilities
```

```jsonc
{
  "version": 1, // protocol major version
  "directWriteMaxBytes": 33554432, // max body for PUT /file (0 = unlimited)
  "multipart": { // omit if unsupported
    "minPartBytes": 5242880,
    "maxPartBytes": 5368709120,
    "maxParts": 10000
  },
  "readRedirect": true, // reads may answer 307 (informational)
  "copy": true, // POST /copy supported
  "maxFileBytes": 53687091200 // optional; absolute object size cap
}
```

Fetched once per mount (at worker boot, lazily on first fs op) and cached for
the worker's lifetime. `version` MUST be `1`; the client refuses to mount
mismatched majors.

If `multipart` is omitted, writes larger than `directWriteMaxBytes` fail with
`EFBIG`. If `copy` is `false`/omitted, the client emulates copy via read+write
(and documents the cost).

## 5. Endpoints

Required unless marked optional.

### 5.1 `GET /stat?path=`

Returns the `Entry` for the path. `404 NotFound` if absent. This is the hot
endpoint — implementations should make it one lookup, not a per-segment walk.

### 5.2 `GET /list?path=&cursor=&limit=`

Lists a directory. Returns:

```jsonc
{ "entries": [Entry, ...], "cursor": "opaque-or-null" }
```

- `NotFound` if the path doesn't exist, `NotADirectory` if it's a file.
- `limit` is a hint; servers may return fewer. `cursor` is opaque; `null` or
  absent means done. Order should be stable across pages (name order
  recommended) but the client does not depend on a specific collation.

### 5.3 `GET /read?path=`

Returns the file bytes, one of:

- `200` with the raw body (`content-type` set if known), or
- `307` with `location:` pointing at a URL that serves the bytes (e.g. a
  presigned URL). The client follows one level of redirect.

The client MAY send a `range:` header (single `bytes=a-b` ranges only); the
server (or its redirect target) MUST honor it with `206`/`content-range`. Ranged
reads back seek/partial-read on open file handles — they are the common case,
not the exception.

`IsADirectory` for dirs, `NotFound` if absent.

### 5.4 `PUT /write?path=&contentType=&overwrite=`

Direct write: the raw body becomes the file's content, atomically — readers see
the old content or the new, never a torn write.

- Creates parent directories implicitly (the client relies on this for
  `writeFile` after `mkdir -p` races).
- `overwrite` defaults to `true`; with `overwrite=false` an existing file →
  `AlreadyExists` (backs `O_EXCL`).
- Bodies over `directWriteMaxBytes` → `TooLarge`; the client won't send them (it
  switches to multipart) but servers must still enforce.
- Response: `200` with the resulting `Entry`.

### 5.5 Multipart upload (optional, capability-gated)

For bodies over `directWriteMaxBytes`. Three steps, mirroring the staged model:

```
POST /upload?path=&contentType=&sizeHint=
→ { "uploadId": "opaque", "partUrls": null | [ ... ] }

POST /upload/part?uploadId=&partNumber=&size=
→ { "url": "https://...", "expiresAtMs": 1730000000000 }
   (client PUTs the part bytes to `url` — same-origin: configured headers/query
    attached; cross-origin e.g. presigned: omitted. Response's `etag` header is
    retained per part)

POST /upload/commit?uploadId=
  body: { "parts": [ { "partNumber": 1, "etag": "..." }, ... ] }
→ 200 with the resulting Entry (file becomes visible at `path` only now)

DELETE /upload?uploadId=
→ abort; discards buffered parts. Idempotent.
```

Uncommitted uploads MUST eventually be garbage-collected server-side; the client
aborts on failure paths but cannot guarantee delivery.

### 5.6 `POST /mkdir?path=&parents=`

Creates a directory. `parents=true` creates missing ancestors (`mkdir -p`) and
is not an error if the path already exists as a dir. Without `parents`, missing
ancestor → `NotFound`, existing path → `AlreadyExists`. Response: `200` with the
`Entry`.

Backends where directories are virtual (pure key prefixes) MAY implement mkdir
as a no-op returning a synthetic Entry — but then `stat`/`list` must be
consistent with that fiction.

### 5.7 `DELETE /remove?path=&recursive=`

Deletes a file or directory. Non-recursive delete of a non-empty dir →
`NotEmpty`. `recursive=true` removes the subtree. `204` on success; deleting a
non-existent path → `NotFound` (the client maps `unlink` ENOENT semantics).

### 5.8 `POST /move`

Body: `{ "from": "/a/b.txt", "to": "/c/d.txt", "overwrite": false }`

Renames/moves a file or directory (with its subtree). `overwrite=false` +
existing destination → `AlreadyExists`. Moving into a non-existent parent →
`NotFound`. Response: `200` with the destination `Entry`.

### 5.9 `POST /copy` (optional, capability-gated)

Body: `{ "from": "/a/b.txt", "to": "/c/d.txt", "overwrite": false }`

Copies a file (dirs: servers MAY support recursive copy; if not,
`IsADirectory`). Response: `200` with the destination `Entry`.

## 6. Client behavior guarantees (informative)

What servers can assume about the runtime's client:

- Normalizes paths before sending; never sends `..`.
- Caches `stat`/`list` results briefly (sub-second TTL) with write-through
  invalidation; servers must still be the source of truth.
- Retries idempotent requests (`stat`, `list`, `read`, `remove`, `move` is NOT
  retried) on 5xx/429 with jittered exponential backoff, bounded by the worker's
  remaining wall-clock budget.
- Buffers writes locally and flushes on file close/`fsync` — a `writeFile` is
  one `PUT /write`, not a stream of small writes.
- Fs surface with no protocol mapping (symlinks, hardlinks, chmod/chown, watch)
  is faked or rejected client-side and never reaches the server.

## 7. Runtime configuration

```jsonc
FlowRuntime.userWorkers.create({
  // ...
  httpFs: [
    {
      mountPoint: "/objects",
      baseUrl: "https://api.example.com/fs/v1",
      headers: { Authorization: "Bearer <opaque>" },
      query: { wsId: "<id>" }, // optional; both maps are optional
    },
    {
      // A server on the same host, reached over a unix socket (§1.2).
      mountPoint: "/local",
      baseUrl: "http://localhost/fs/v1", // placeholder host; path prefix + origin
      socketPath: "/run/flow/fs.sock",
    },
  ],
})
```

Multiple mounts are allowed; mount points must not nest inside one another or
collide with `/tmp` or any configured S3 mount. Sync fs APIs work on HttpFS
mounts (each sync call blocks the calling worker thread for at least one network
round trip — worker wall-clock timeouts keep running).

`socketPath` (optional) routes the mount over an AF_UNIX socket instead of TCP;
see §1.2 for how `baseUrl` is interpreted in that case.

## 8. Conformance

The conformance suite (`edge/crates/fs/tests/httpfs_conformance.rs`) runs the
Rust client against an in-process mock server that acts as the reference
implementation of this protocol. It exercises every endpoint, the error/errno
mappings, custom headers and query params (including same-origin attachment and
cross-origin stripping), redirect handling, pagination, ranged reads, overwrite
flags, capability gating (version refusal, copy fallback, multipart vs.
`EFBIG`), and the sync fs surface. It also serves the same mock over an AF_UNIX
socket (§1.2) to cover the unix-socket transport end-to-end, including
same-origin redirect routing. Run it with:

```
cargo test -p fs --test httpfs_conformance
```

Server implementers should treat the mock's behavior in that file as the
executable companion to this document: a server that answers each request the
way the mock does is conformant.
