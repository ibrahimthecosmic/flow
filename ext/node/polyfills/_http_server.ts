// Copyright 2018-2026 the Deno authors. MIT license.

// TODO(petamoriken): enable prefer-primordials for node polyfills
// deno-lint-ignore-file
// trex: server-side cluster carved from the former monolithic http.ts onto
// upstream 2.7.14's modular layout. Some imports above are client-only and
// unused here; lint is relaxed pending a follow-up prune.

import { core, internals, primordials } from "ext:core/mod.js";
import {
  op_node_http_await_information,
  op_node_http_await_response,
  op_node_http_fetch_response_upgrade,
  op_node_http_request_with_conn,
  op_node_http_response_reclaim_conn,
  op_tls_key_null,
  op_tls_key_static,
  op_tls_start,
} from "ext:core/ops";

import { TextEncoder } from "ext:deno_web/08_text_encoding.js";
import { setTimeout } from "ext:deno_web/02_timers.js";
import {
  _normalizeArgs,
  createConnection,
  ListenOptions,
  Socket,
} from "node:net";
import { Buffer } from "node:buffer";
import { ERR_SERVER_NOT_RUNNING } from "ext:deno_node/internal/errors.ts";
import { EventEmitter } from "node:events";
import { nextTick } from "ext:deno_node/_next_tick.ts";
import {
  validateAbortSignal,
  validateBoolean,
  validateInteger,
  validateObject,
  validatePort,
} from "ext:deno_node/internal/validators.mjs";
import {
  addAbortSignal,
  Duplex as NodeDuplex,
  finished,
  Readable as NodeReadable,
  Writable as NodeWritable,
  WritableOptions as NodeWritableOptions,
} from "node:stream";
import {
  kUniqueHeaders,
  OutgoingMessage,
  parseUniqueHeadersOption,
  validateHeaderName,
  validateHeaderValue,
} from "node:_http_outgoing";
import { ok as assert } from "node:assert";
import { Agent } from "node:_http_agent";
import { kEmptyObject, once } from "ext:deno_node/internal/util.mjs";
import { constants, TCP } from "ext:deno_node/internal_binding/tcp_wrap.ts";
import {
  connResetException,
  ERR_HTTP_HEADERS_SENT,
  ERR_HTTP_SOCKET_ASSIGNED,
  ERR_INVALID_ARG_TYPE,
  ERR_INVALID_HTTP_TOKEN,
  ERR_INVALID_PROTOCOL,
  ERR_UNESCAPED_CHARACTERS,
} from "ext:deno_node/internal/errors.ts";
import { getIPFamily } from "ext:deno_node/internal/net.ts";
import { upgradeHttpRaw as defaultUpgradeHttpRaw } from "ext:deno_http/00_serve.ts";
import { op_http_serve_address_override } from "ext:core/ops";
import { serve } from "ext:runtime/http.js";
import { headersEntries } from "ext:deno_fetch/20_headers.js";
import { Response } from "ext:deno_fetch/23_response.js";
import {
  builtinTracer,
  ContextManager,
  enterSpan,
  PROPAGATORS,
  restoreSnapshot,
  TRACING_ENABLED,
} from "ext:deno_telemetry/telemetry.ts";
import {
  kDestroyed,
  kEnded,
  kEnding,
  kErrored,
  kState,
} from "ext:deno_node/internal/streams/utils.js";
import { deprecate } from "node:util";

// Flag to track if DENO_SERVE_ADDRESS override has been consumed for Node http servers.
let nodeHttpAddressOverrideConsumed = false;

const { internalRidSymbol } = core;
const {
  ArrayIsArray,
  ArrayPrototypePush,
  ObjectDefineProperty,
  StringPrototypeIncludes,
  StringPrototypeToLowerCase,
  SafeArrayIterator,
} = primordials;

type Chunk = string | Buffer | Uint8Array;

const ENCODER = new TextEncoder();

export interface RequestOptions {
  agent?: Agent;
  auth?: string;
  createConnection?: () => unknown;
  defaultPort?: number;
  family?: number;
  headers?: Record<string, string>;
  hints?: number;
  host?: string;
  hostname?: string;
  insecureHTTPParser?: boolean;
  localAddress?: string;
  localPort?: number;
  lookup?: () => void;
  maxHeaderSize?: number;
  method?: string;
  path?: string;
  port?: number;
  protocol?: string;
  setHost?: boolean;
  socketPath?: string;
  timeout?: number;
  signal?: AbortSignal;
  href?: string;
}

function validateHost(host, name) {
  if (host !== null && host !== undefined && typeof host !== "string") {
    throw new ERR_INVALID_ARG_TYPE(`options.${name}`, [
      "string",
      "undefined",
      "null",
    ], host);
  }
  return host;
}

const INVALID_PATH_REGEX = /[^\u0021-\u00ff]/;
const kError = Symbol("kError");
const kBindToAbortSignal = Symbol("kBindToAbortSignal");

class FakeSocket extends EventEmitter {
  /** Stores the underlying request for lazily binding to abort signal */
  #request: Request | undefined;
  constructor(
    opts: {
      encrypted?: boolean | undefined;
      remotePort?: number | undefined;
      remoteAddress?: string | undefined;
      reader?: ReadableStreamDefaultReader | undefined;
      request?: Request;
    } = {},
  ) {
    super();
    this.remoteAddress = opts.remoteAddress;
    this.remotePort = opts.remotePort;
    this.encrypted = opts.encrypted;
    this.reader = opts.reader;
    this.writable = true;
    this.readable = true;
    this.#request = opts.request;
  }

  [kBindToAbortSignal]() {
    const signal = this.#request?.signal;
    signal?.addEventListener("abort", () => {
      this.emit("error", signal.reason);
      this.emit("close");
    }, { once: true });
  }

  setKeepAlive() {}

  end() {}

  destroy() {}

  setTimeout(callback, timeout = 0, ...args) {
    setTimeout(callback, timeout, args);
  }
}

function emitErrorEvent(request, error) {
  request.emit("error", error);
}

// trex: STATUS_CODES inlined (was imported from node:_http_server, which now
// resolves to this very module).
export const STATUS_CODES = {
  100: "Continue",
  101: "Switching Protocols",
  102: "Processing",
  103: "Early Hints",
  200: "OK",
  201: "Created",
  202: "Accepted",
  203: "Non-Authoritative Information",
  204: "No Content",
  205: "Reset Content",
  206: "Partial Content",
  207: "Multi-Status",
  208: "Already Reported",
  226: "IM Used",
  300: "Multiple Choices",
  301: "Moved Permanently",
  302: "Found",
  303: "See Other",
  304: "Not Modified",
  305: "Use Proxy",
  307: "Temporary Redirect",
  308: "Permanent Redirect",
  400: "Bad Request",
  401: "Unauthorized",
  402: "Payment Required",
  403: "Forbidden",
  404: "Not Found",
  405: "Method Not Allowed",
  406: "Not Acceptable",
  407: "Proxy Authentication Required",
  408: "Request Timeout",
  409: "Conflict",
  410: "Gone",
  411: "Length Required",
  412: "Precondition Failed",
  413: "Payload Too Large",
  414: "URI Too Long",
  415: "Unsupported Media Type",
  416: "Range Not Satisfiable",
  417: "Expectation Failed",
  418: "I'm a Teapot",
  421: "Misdirected Request",
  422: "Unprocessable Entity",
  423: "Locked",
  424: "Failed Dependency",
  425: "Too Early",
  426: "Upgrade Required",
  428: "Precondition Required",
  429: "Too Many Requests",
  431: "Request Header Fields Too Large",
  451: "Unavailable For Legal Reasons",
  500: "Internal Server Error",
  501: "Not Implemented",
  502: "Bad Gateway",
  503: "Service Unavailable",
  504: "Gateway Timeout",
  505: "HTTP Version Not Supported",
  506: "Variant Also Negotiates",
  507: "Insufficient Storage",
  508: "Loop Detected",
  509: "Bandwidth Limit Exceeded",
  510: "Not Extended",
  511: "Network Authentication Required",
};

function onError(self, error, cb) {
  // This is to keep backward compatible behavior.
  // An error is emitted only if there are listeners attached to the event.
  if (self.listenerCount("error") === 0) {
    cb();
  } else {
    cb(error);
  }
}

export type ServerResponse = {
  req: IncomingMessageForServer;
  statusCode: number;
  statusMessage?: string;

  _headers: Record<string, string | string[]>;
  _hasNonStringHeaders: boolean;

  _readable: ReadableStream;
  finished: boolean;
  headersSent: boolean;
  _resolve: (value: Response | PromiseLike<Response>) => void;
  // deno-lint-ignore no-explicit-any
  _socketOverride: any | null;
  // deno-lint-ignore no-explicit-any
  socket: any | null;

  setHeader(name: string, value: string | string[]): void;
  appendHeader(name: string, value: string | string[]): void;
  getHeader(name: string): string | string[];
  removeHeader(name: string): void;
  getHeaderNames(): string[];
  getHeaders(): Record<string, string | number | string[]>;
  hasHeader(name: string): boolean;

  writeHead(
    status: number,
    statusMessage?: string,
    headers?:
      | Record<string, string | number | string[]>
      | Array<[string, string]>,
  ): void;
  writeHead(
    status: number,
    headers?:
      | Record<string, string | number | string[]>
      | Array<[string, string]>,
  ): void;

  _ensureHeaders(singleChunk?: Chunk): void;

  respond(final: boolean, singleChunk?: Chunk): void;
  // deno-lint-ignore no-explicit-any
  end(chunk?: any, encoding?: any, cb?: any): void;

  flushHeaders(): void;
  writeEarlyHints(
    hints: Record<string, string | string[]>,
    callback?: () => void,
  ): void;
  _implicitHeader(): void;

  // Undocumented field used by `npm:light-my-request`.
  _header: string;

  assignSocket(socket): void;
  detachSocket(socket): void;
} & { -readonly [K in keyof NodeWritable]: NodeWritable[K] };

type ServerResponseStatic = {
  new (
    resolve: (value: Response | PromiseLike<Response>) => void,
    socket: FakeSocket,
  ): ServerResponse;
  _enqueue(controller: ReadableStreamDefaultController, chunk: Chunk): void;
  _bodyShouldBeNull(statusCode: number): boolean;
};

export const ServerResponse = function (
  this: ServerResponse,
  req: IncomingMessageForServer,
  resolve: (value: Response | PromiseLike<Response>) => void,
  socket: FakeSocket,
) {
  this.req = req;
  this.statusCode = 200;
  this.statusMessage = undefined;
  this._headers = { __proto__: null };
  this._hasNonStringHeaders = false;
  this.writable = true;

  // used by `npm:on-finished`
  this.finished = false;
  this.headersSent = false;
  this._socketOverride = null;

  let controller: ReadableByteStreamController;
  const readable = new ReadableStream({
    start(c) {
      controller = c as ReadableByteStreamController;
    },
  });

  NodeWritable.call(
    this,
    {
      autoDestroy: true,
      defaultEncoding: "utf-8",
      emitClose: true,
      // FIXME: writes don't work when a socket is assigned and then
      // detached.
      write: (chunk, encoding, cb) => {
        // Writes chunks are directly written to the socket if
        // one is assigned via assignSocket()
        if (this._socketOverride && this._socketOverride.writable) {
          this._socketOverride.write(chunk, encoding);
          return cb();
        }
        if (!this.headersSent) {
          ServerResponse._enqueue(controller, chunk);
          this.respond(false);
          return cb();
        }
        ServerResponse._enqueue(controller, chunk);
        return cb();
      },
      final: (cb) => {
        if (!this.headersSent) {
          this.respond(true);
        }
        controller.close();
        return cb();
      },
      destroy: (err, cb) => {
        if (err) {
          controller.error(err);
        }
        return cb(null);
      },
    } satisfies NodeWritableOptions,
  );

  this._readable = readable;
  this._resolve = resolve;
  this.socket = socket;
  this.on("newListener", (event) => {
    if (event === "close") {
      this.socket?.[kBindToAbortSignal]();
      this.socket?.on("close", () => {
        if (!this.finished) {
          this.emit("close");
        }
      });
    }
  });
  this._header = "";
} as unknown as ServerResponseStatic;

Object.setPrototypeOf(ServerResponse.prototype, NodeWritable.prototype);
Object.setPrototypeOf(ServerResponse, NodeWritable);

ServerResponse._enqueue = function (
  this: ServerResponse,
  controller: ReadableStreamDefaultController,
  chunk: Chunk,
) {
  try {
    if (typeof chunk === "string") {
      controller.enqueue(ENCODER.encode(chunk));
    } else {
      controller.enqueue(chunk);
    }
  } catch (_) {
    // The stream might have been closed. Ignore the error.
  }
};

/** Returns true if the response body should be null with the given
 * http status code */
ServerResponse._bodyShouldBeNull = function (
  this: ServerResponse,
  status: number,
) {
  return status === 101 || status === 204 || status === 205 || status === 304;
};

ServerResponse.prototype.setHeader = function (
  this: ServerResponse,
  name: string,
  value: string | string[],
) {
  if (Array.isArray(value)) {
    this._hasNonStringHeaders = true;
  }
  this._headers[StringPrototypeToLowerCase(name)] = value;
  return this;
};

ServerResponse.prototype.setHeaders = function setHeaders(
  this: ServerResponse,
  headers: Headers | Map<string, string | string[]>,
) {
  if (this._header) {
    throw new ERR_HTTP_HEADERS_SENT("set");
  }

  if (
    !headers ||
    ArrayIsArray(headers) ||
    typeof headers.keys !== "function" ||
    typeof headers.get !== "function"
  ) {
    throw new ERR_INVALID_ARG_TYPE("headers", ["Headers", "Map"], headers);
  }

  // Headers object joins multiple cookies with a comma when using
  // the getter to retrieve the value,
  // unless iterating over the headers directly.
  // We also cannot safely split by comma.
  // To avoid setHeader overwriting the previous value we push
  // set-cookie values in array and set them all at once.
  const cookies = [];

  for (const { 0: key, 1: value } of headers) {
    if (key === "set-cookie") {
      if (ArrayIsArray(value)) {
        cookies.push(...value);
      } else {
        cookies.push(value);
      }
      continue;
    }
    this.setHeader(key, value);
  }
  if (cookies.length) {
    this.setHeader("set-cookie", cookies);
  }

  return this;
};

ServerResponse.prototype.appendHeader = function (
  this: ServerResponse,
  name: string,
  value: string | string[],
) {
  const key = StringPrototypeToLowerCase(name);
  if (this._headers[key] === undefined) {
    if (Array.isArray(value)) this._hasNonStringHeaders = true;
    this._headers[key] = value;
  } else {
    this._hasNonStringHeaders = true;
    if (!Array.isArray(this._headers[key])) {
      this._headers[key] = [this._headers[key]];
    }
    const header = this._headers[key];
    if (Array.isArray(value)) {
      header.push(...value);
    } else {
      header.push(value);
    }
  }
  return this;
};

ServerResponse.prototype.getHeader = function (
  this: ServerResponse,
  name: string,
) {
  return this._headers[StringPrototypeToLowerCase(name)];
};

ServerResponse.prototype.removeHeader = function (
  this: ServerResponse,
  name: string,
) {
  delete this._headers[StringPrototypeToLowerCase(name)];
};

ServerResponse.prototype.getHeaderNames = function (this: ServerResponse) {
  return Object.keys(this._headers);
};

ServerResponse.prototype.getHeaders = function (
  this: ServerResponse,
): Record<string, string | number | string[]> {
  return { __proto__: null, ...this._headers };
};

ServerResponse.prototype.hasHeader = function (
  this: ServerResponse,
  name: string,
) {
  return Object.hasOwn(this._headers, StringPrototypeToLowerCase(name));
};

ServerResponse.prototype.writeHead = function (
  this: ServerResponse,
  status: number,
  statusMessageOrHeaders?:
    | string
    | Record<string, string | number | string[]>
    | Array<[string, string]>
    | Array<string>,
  maybeHeaders?:
    | Record<string, string | number | string[]>
    | Array<[string, string]>
    | Array<string>,
) {
  this.statusCode = status;

  let headers = null;
  if (typeof statusMessageOrHeaders === "string") {
    this.statusMessage = statusMessageOrHeaders;
    if (maybeHeaders !== undefined) {
      headers = maybeHeaders;
    }
  } else if (statusMessageOrHeaders !== undefined) {
    headers = statusMessageOrHeaders;
  }

  if (headers !== null) {
    if (ArrayIsArray(headers)) {
      headers = headers as Array<[string, string]> | Array<string>;

      // Headers should override previous headers but still
      // allow explicit duplicates. To do so, we first remove any
      // existing conflicts, then use appendHeader.

      if (ArrayIsArray(headers[0])) {
        headers = headers as Array<[string, string]>;
        for (let i = 0; i < headers.length; i++) {
          const headerTuple = headers[i];
          const k = headerTuple[0];
          if (k) this.removeHeader(k);
        }

        for (let i = 0; i < headers.length; i++) {
          const headerTuple = headers[i];
          const k = headerTuple[0];
          if (k) this.appendHeader(k, headerTuple[1]);
        }
      } else {
        headers = headers as Array<string>;
        for (let i = 0; i < headers.length; i += 2) {
          const k = headers[i];
          this.removeHeader(k);
        }

        for (let i = 0; i < headers.length; i += 2) {
          const k = headers[i];
          if (k) this.appendHeader(k, headers[i + 1]);
        }
      }
    } else {
      headers = headers as Record<string, string>;
      for (const k in headers) {
        if (Object.hasOwn(headers, k)) {
          this.setHeader(k, headers[k]);
        }
      }
    }
  }

  return this;
};

ServerResponse.prototype._ensureHeaders = function (
  this: ServerResponse,
  singleChunk?: Chunk,
) {
  if (this.statusCode === 200 && this.statusMessage === undefined) {
    this.statusMessage = "OK";
  }
  if (typeof singleChunk === "string" && !this.hasHeader("content-type")) {
    this.setHeader("content-type", "text/plain;charset=UTF-8");
  }
};

ServerResponse.prototype.respond = function (
  this: ServerResponse,
  final: boolean,
  singleChunk?: Chunk,
) {
  this.headersSent = true;
  this._ensureHeaders(singleChunk);
  let body = singleChunk ?? (final ? null : this._readable);
  if (ServerResponse._bodyShouldBeNull(this.statusCode)) {
    body = null;
  }
  let headers: Record<string, string> | [string, string][] = this
    ._headers as Record<string, string>;
  if (this._hasNonStringHeaders) {
    headers = [];
    // Guard is not needed as this is a null prototype object.
    // deno-lint-ignore guard-for-in
    for (const key in this._headers) {
      const entry = this._headers[key];
      if (Array.isArray(entry)) {
        for (const value of entry) {
          headers.push([key, value]);
        }
      } else {
        headers.push([key, entry]);
      }
    }
  }
  this._resolve(
    new Response(body, {
      headers,
      status: this.statusCode,
      statusText: this.statusMessage,
    }),
  );
};

ServerResponse.prototype.end = function (
  this: ServerResponse,
  // deno-lint-ignore no-explicit-any
  chunk?: any,
  // deno-lint-ignore no-explicit-any
  encoding?: any,
  // deno-lint-ignore no-explicit-any
  cb?: any,
) {
  this.finished = true;
  if (!chunk && "transfer-encoding" in this._headers) {
    // FIXME(bnoordhuis) Node sends a zero length chunked body instead, i.e.,
    // the trailing "0\r\n", but respondWith() just hangs when I try that.
    this._headers["content-length"] = "0";
    delete this._headers["transfer-encoding"];
  }

  // @ts-expect-error The signature for cb is stricter than the one implemented here
  NodeWritable.prototype.end.call(this, chunk, encoding, cb);
};

ServerResponse.prototype.flushHeaders = function (this: ServerResponse) {
  // no-op
};

// Undocumented API used by `npm:compression`.
ServerResponse.prototype._implicitHeader = function (this: ServerResponse) {
  this.writeHead(this.statusCode);
};

ServerResponse.prototype.assignSocket = function (
  this: ServerResponse,
  socket,
) {
  if (socket._httpMessage) {
    throw new ERR_HTTP_SOCKET_ASSIGNED();
  }
  socket._httpMessage = this;
  this._socketOverride = socket;
};

ServerResponse.prototype.detachSocket = function (
  this: ServerResponse,
  socket,
) {
  assert(socket._httpMessage === this);
  socket._httpMessage = null;
  this._socketOverride = null;
};

ServerResponse.prototype.writeContinue = function writeContinue(cb) {
  if (cb) {
    nextTick(cb);
  }
};

ServerResponse.prototype.writeEarlyHints = function writeEarlyHints(
  _hints,
  cb,
) {
  if (cb) {
    nextTick(cb);
  }
};

Object.defineProperty(ServerResponse.prototype, "connection", {
  get: deprecate(
    function (this: ServerResponse) {
      return this._socketOverride;
    },
    "ServerResponse.prototype.connection is deprecated",
    "DEP0066",
  ),
  set: deprecate(
    // deno-lint-ignore no-explicit-any
    function (this: ServerResponse, socket: any) {
      this._socketOverride = socket;
    },
    "ServerResponse.prototype.connection is deprecated",
    "DEP0066",
  ),
});

const kRawHeaders = Symbol("rawHeaders");

// TODO(@AaronO): optimize
export class IncomingMessageForServer extends NodeReadable {
  #headers: Record<string, string>;
  url: string;
  method: string;
  socket: Socket | FakeSocket;

  constructor(socket: FakeSocket | Socket) {
    const reader = socket instanceof FakeSocket
      ? socket.reader
      : socket instanceof Socket
      ? NodeDuplex.toWeb(socket).readable.getReader()
      : null;
    super({
      autoDestroy: true,
      emitClose: true,
      objectMode: false,
      read: async function (_size) {
        if (!reader) {
          return this.push(null);
        }

        try {
          const { value } = await reader!.read();
          this.push(value !== undefined ? Buffer.from(value) : null);
        } catch (err) {
          this.destroy(err as Error);
        }
      },
      destroy: (err, cb) => {
        reader?.cancel().catch(() => {
          // Don't throw error - it's propagated to the user via 'error' event.
        }).finally(nextTick(onError, this, err, cb));
      },
    });
    this.url = "";
    this.method = "";
    this.socket = socket;
    this.upgrade = null;
    this[kRawHeaders] = [];
    socket?.on("error", (e) => {
      if (this.listenerCount("error") > 0) {
        this.emit("error", e);
      }
    });
  }

  get aborted() {
    return false;
  }

  get httpVersion() {
    return "1.1";
  }

  set httpVersion(val) {
    assert(val === "1.1");
  }

  get headers() {
    if (!this.#headers) {
      this.#headers = {};
      const entries = headersEntries(this[kRawHeaders]);
      for (let i = 0; i < entries.length; i++) {
        const entry = entries[i];
        this.#headers[entry[0]] = entry[1];
      }
    }
    return this.#headers;
  }

  set headers(val) {
    this.#headers = val;
  }

  get rawHeaders() {
    const entries = headersEntries(this[kRawHeaders]);
    const out = new Array(entries.length * 2);
    for (let i = 0; i < entries.length; i++) {
      out[i * 2] = entries[i][0];
      out[i * 2 + 1] = entries[i][1];
    }
    return out;
  }

  // connection is deprecated, but still tested in unit test.
  get connection() {
    return this.socket;
  }

  setTimeout(msecs, callback) {
    if (callback) {
      this.on("timeout", callback);
    }
    this.socket.setTimeout(msecs);
    return this;
  }
}

export type ServerHandler = (
  req: IncomingMessageForServer,
  res: ServerResponse,
) => void;

export function Server(opts, requestListener?: ServerHandler): ServerImpl {
  return new ServerImpl(opts, requestListener);
}

function _addAbortSignalOption(server: ServerImpl, options: ListenOptions) {
  if (options?.signal === undefined) {
    return;
  }

  validateAbortSignal(options.signal, "options.signal");
  const { signal } = options;

  const onAborted = () => {
    server.close();
  };

  if (signal.aborted) {
    nextTick(onAborted);
  } else {
    signal.addEventListener("abort", onAborted);
    server.once("close", () => signal.removeEventListener("abort", onAborted));
  }
}

export class ServerImpl extends EventEmitter {
  #addr: Deno.NetAddr | null = null;
  #hasClosed = false;
  #server: Deno.HttpServer;
  #unref = false;
  #ac?: AbortController;
  #listener: Deno.Listener | null = null;
  #serveDeferred: ReturnType<typeof Promise.withResolvers<void>>;
  listening = false;

  constructor(opts, requestListener?: ServerHandler) {
    super();

    if (typeof opts === "function") {
      requestListener = opts;
      opts = kEmptyObject;
    } else if (opts == null) {
      opts = kEmptyObject;
    } else {
      validateObject(opts, "options");
    }

    this._opts = opts;

    this.#serveDeferred = Promise.withResolvers<void>();
    this.#serveDeferred.promise.then(() => this.emit("close"));
    if (requestListener !== undefined) {
      this.on("request", requestListener);
    }
  }

  listen(...args: unknown[]): this {
    // TODO(bnoordhuis) Delegate to net.Server#listen().
    const normalized = _normalizeArgs(args);
    const options = normalized[0] as Partial<ListenOptions>;
    const cb = normalized[1];

    if (cb !== null) {
      // @ts-ignore change EventEmitter's sig to use CallableFunction
      this.once("listening", cb);
    }

    let port = 0;
    if (typeof options.port === "number" || typeof options.port === "string") {
      validatePort(options.port, "options.port");
      port = options.port | 0;
    }

    _addAbortSignalOption(this, options);

    // TODO(bnoordhuis) Node prefers [::] when host is omitted,
    // we on the other hand default to 0.0.0.0.
    let hostname = options.host ?? "0.0.0.0";
    if (hostname == "localhost") {
      hostname = "127.0.0.1";
    }

    // Check DENO_SERVE_ADDRESS override (used by desktop runtime, Deno Deploy, etc.)
    if (!nodeHttpAddressOverrideConsumed) {
      const {
        0: overrideKind,
        1: overrideHost,
        2: overridePort,
      } = op_http_serve_address_override();
      if (overrideKind === 1) {
        // TCP override
        nodeHttpAddressOverrideConsumed = true;
        hostname = overrideHost;
        port = overridePort;
      }
    }

    // Bind the port synchronously so that address() returns the actual
    // port immediately after listen(), matching Node.js behavior.
    try {
      this.#listener = this._listen(hostname, port);
    } catch (e) {
      // Emit the error asynchronously, matching Node.js behavior.
      this.#addr = { hostname, port } as Deno.NetAddr;
      nextTick(() => this.emit("error", e));
      return this;
    }
    const addr = this.#listener.addr as Deno.NetAddr;
    this.#addr = {
      hostname: addr.hostname,
      port: addr.port,
    } as Deno.NetAddr;
    this.listening = true;
    nextTick(() => this._serve());

    return this;
  }

  _listen(
    hostname: string,
    port: number,
  ): { addr: Deno.NetAddr; close(): void } {
    // trex's serve() does its own Deno.listen() internally (ext/runtime/js/http.js),
    // so don't bind here - return a fake listener whose .addr is read synchronously
    // by the caller to populate this.#addr.
    return {
      addr: { hostname, port, transport: "tcp" } as Deno.NetAddr,
      close() {},
    };
  }

  _serve() {
    const ac = new AbortController();
    const handler = (request: Request, info: Deno.ServeHandlerInfo) => {
      const socket = new FakeSocket({
        remoteAddress: info.remoteAddr.hostname,
        remotePort: info.remoteAddr.port,
        encrypted: this._encrypted,
        reader: request.body?.getReader(),
        request,
      });

      const req = new IncomingMessageForServer(socket);
      req.method = request.method;

      if (request.method === "CONNECT") {
        // For CONNECT, the URL should be in authority form (host:port).
        // Deno's server adds an "http://" prefix, so strip it.
        req.url = request.url.replace(/^https?:\/\//, "");
        req[kRawHeaders] = request.headers;

        // TODO(trex): CONNECT upgrades need the fence-based raw-upgrade path
        // used by ext/runtime/js/http.js (op_http_upgrade_raw2_fence). Not
        // wired through trex's serve() yet - reject until a test needs it.
        return new Response(null, { status: 501 });
      }

      // Slice off the origin so that we only have pathname + search
      req.url = request.url?.slice(request.url.indexOf("/", 8));
      req.upgrade =
        request.headers.get("connection")?.toLowerCase().includes("upgrade") &&
        request.headers.get("upgrade");
      req[kRawHeaders] = request.headers;
      // trex/edge-runtime: embedder servers (e.g. express via
      // http.createServer().listen()) re-parent this request with
      // `Object.setPrototypeOf(req, app.request)`, which removes
      // IncomingMessageForServer.prototype's `headers`/`rawHeaders` getters from
      // the chain. After that, `req.headers` resolves through node:http's
      // IncomingMessage (_http_incoming.js), whose getter reads `this.rawHeaders`
      // + the private `kHeadersCount` symbol - neither of which exist on this
      // instance - so `req.headers` comes back empty. (In the pre-2.7.14
      // monolithic stack the server and node:http IncomingMessage were the same
      // class, so re-parenting was harmless.) Materialize `headers`/`rawHeaders`
      // as own data properties so they survive re-parenting and shadow any
      // prototype getter regardless of which IncomingMessage class is in scope.
      {
        const entries = headersEntries(request.headers);
        const rawHeaders = [];
        const headers = { __proto__: null };
        for (let i = 0; i < entries.length; i++) {
          const name = entries[i][0];
          const value = entries[i][1];
          ArrayPrototypePush(rawHeaders, name, value);
          if (name === "set-cookie") {
            if (headers[name] === undefined) headers[name] = [value];
            else ArrayPrototypePush(headers[name], value);
          } else if (headers[name] === undefined) {
            headers[name] = value;
          } else {
            headers[name] += ", " + value;
          }
        }
        ObjectDefineProperty(req, "headers", {
          __proto__: null,
          value: headers,
          writable: true,
          enumerable: true,
          configurable: true,
        });
        ObjectDefineProperty(req, "rawHeaders", {
          __proto__: null,
          value: rawHeaders,
          writable: true,
          enumerable: true,
          configurable: true,
        });
      }

      // Don't fire the "upgrade" event for h2c (HTTP/2 cleartext) upgrades.
      // These are protocol-level upgrades that aren't meant for user-space
      // handlers (like WebSocket). Treating them as regular requests lets
      // the server respond normally with HTTP/1.1.
      if (
        req.upgrade && req.upgrade.toLowerCase() !== "h2c" &&
        this.listenerCount("upgrade") > 0
      ) {
        // Use internals.upgradeHttpRaw so embedders (e.g. the trexas
        // edge-runtime fork) can override the upgrade implementation.
        // Default deno_http upgradeHttpRaw only works for requests served
        // by Deno.serve; embedders that drive HTTP via the legacy HttpConn
        // API need to substitute their own upgrade flow.
        const upgradeFn = (typeof internals.upgradeHttpRaw === "function")
          ? internals.upgradeHttpRaw
          : defaultUpgradeHttpRaw;
        const { conn, response } = upgradeFn(request);
        const socket = new Socket({
          handle: new TCP(constants.SERVER, conn),
        });
        // Update socket held by `req`.
        req.socket = socket;
        this.emit("upgrade", req, socket, Buffer.from([]));
        return response;
      } else {
        return new Promise<Response>((resolve): void => {
          const res = new ServerResponse(req, resolve, socket);

          if (request.headers.has("expect")) {
            if (/(?:^|\W)100-continue(?:$|\W)/i.test(req.headers.expect)) {
              if (this.listenerCount("checkContinue") > 0) {
                this.emit("checkContinue", req, res);
              } else {
                res.writeContinue();
                this.emit("request", req, res);
              }
            } else if (this.listenerCount("checkExpectation") > 0) {
              this.emit("checkExpectation", req, res);
            } else {
              res.writeHead(417);
              res.end();
            }
          } else {
            this.emit("request", req, res);
          }
        });
      }
    };

    if (this.#hasClosed) {
      return;
    }
    this.#ac = ac;
    const listener = this.#listener;
    this.#listener = null;
    if (!listener) {
      return;
    }
    const addr = listener.addr as Deno.NetAddr;
    try {
      // Route through trex's worker-safe serve() - it performs its own
      // Deno.listen() internally and handles request dispatch through the
      // worker's HTTP pipeline.
      this.#server = serve({
        handler,
        hostname: addr.hostname,
        port: addr.port,
        onError: (_error) =>
          new Response("Internal Server Error", { status: 500 }),
        onListen: () => this.emit("listening"),
      });
    } catch (e) {
      this.emit("error", e);
      return;
    }

    if (this.#unref) {
      this.#server.unref();
    }
    this.#server.finished.then(() => this.#serveDeferred!.resolve());
  }

  setTimeout() {
    // deno-lint-ignore no-console
    console.error("Not implemented: Server.setTimeout()");
  }

  ref() {
    if (this.#server) {
      this.#server.ref();
    }
    this.#unref = false;

    return this;
  }

  unref() {
    if (this.#server) {
      this.#server.unref();
    }
    this.#unref = true;

    return this;
  }

  close(cb?: (err?: Error) => void): this {
    const listening = this.listening;
    this.listening = false;

    this.#hasClosed = true;
    if (typeof cb === "function") {
      if (listening) {
        this.once("close", cb);
      } else {
        this.once("close", function close() {
          cb(new ERR_SERVER_NOT_RUNNING());
        });
      }
    }

    // Close pre-bound listener if _serve() hasn't consumed it yet.
    if (this.#listener) {
      this.#listener.close();
      this.#listener = null;
    }

    if (listening && this.#ac) {
      if (this.#server) {
        this.#server.shutdown();
      } else if (this.#ac) {
        this.#ac.abort();
        this.#ac = undefined;
      }
    } else {
      this.#serveDeferred!.resolve();
    }

    this.#server = undefined;
    return this;
  }

  closeAllConnections() {
    if (this.#hasClosed) {
      return;
    }
    if (this.#ac) {
      this.#ac.abort();
      this.#ac = undefined;
    }
  }

  closeIdleConnections() {
    if (this.#hasClosed) {
      return;
    }

    if (this.#server) {
      this.#server.shutdown();
    }
  }

  address() {
    if (this.#addr === null) return null;
    const addr = this.#addr.hostname;
    // Match Node.js: family is undefined for non-IP addresses (isIP returns 0)
    const family = getIPFamily(addr);
    return { port: this.#addr.port, address: addr, family };
  }
}

Server.prototype = ServerImpl.prototype;

export function createServer(opts, requestListener?: ServerHandler) {
  return Server(opts, requestListener);
}

// trex: the socket-based connectionListener is part of upstream's net.Server
// HTTP model, which the edge-runtime worker sandbox does not use (requests are
// driven through serve()/op_http_start, not raw TCP accept). Exported only to
// satisfy node:http's re-export surface.
export function _connectionListener() {
  throw new Error(
    "_http_server._connectionListener is not supported in this runtime",
  );
}

// trex: node:http2 / node:https (upstream 2.7.14) import these server-internal
// helpers from node:_http_server. trex's server is serve()-based (not the
// net.Server connection model), so connection tracking is a no-op; storeHTTPOptions
// still records the request/response classes so subclasses can override them.
import { kIncomingMessage } from "node:_http_common";
const kServerResponse = Symbol("ServerResponse");

function setupConnectionsTracking() {}

function httpServerPreClose(server) {
  server?.closeIdleConnections?.();
}

function storeHTTPOptions(options) {
  this[kIncomingMessage] = options.IncomingMessage || IncomingMessageForServer;
  this[kServerResponse] = options.ServerResponse || ServerResponse;
}

export {
  httpServerPreClose,
  kIncomingMessage,
  kServerResponse,
  setupConnectionsTracking,
  storeHTTPOptions,
};

export default {
  STATUS_CODES,
  Server,
  ServerResponse,
  IncomingMessageForServer,
  createServer,
  _connectionListener,
  httpServerPreClose,
  kIncomingMessage,
  kServerResponse,
  setupConnectionsTracking,
  storeHTTPOptions,
};
