import { core, internals, primordials } from "ext:core/mod.js";

// flow(2.9.0 node-compat): Deno 2.9.0 removed `ext:deno_node/00_globals.js`
// and `02_init.js` (the eager `nodeGlobals` model). Node globals are now
// lazy-loaded global properties (matching runtime/js/98_global_scope_shared.js):
// first access to `process`/`Buffer`/`setImmediate` pulls the node:* polyfill,
// which self-bootstraps from `internals.__nodeBootstrapArgs` (stashed at the
// end of bootstrapSBEdge). Workers that never touch node pay nothing.
const lazyProcessMod = core.createLazyLoader("node:process");
const lazyBufferMod = core.createLazyLoader("node:buffer");
const lazyNodeTimersMod = core.createLazyLoader("node:timers");

core.loadExtScript("ext:deno_process/40_process.js");
import "ext:runtime/98_global_scope_shared.js";
core.loadExtScript("ext:deno_http/00_serve.ts");

const abortSignal = core.loadExtScript("ext:deno_web/03_abort_signal.js");
const base64 = core.loadExtScript("ext:deno_web/05_base64.js");
const console = core.loadExtScript("ext:deno_web/01_console.js");
const crypto = core.loadExtScript("ext:deno_crypto/00_crypto.js");
const { DOMException } = core.loadExtScript("ext:deno_web/01_dom_exception.js");
const encoding = core.loadExtScript("ext:deno_web/08_text_encoding.js");
const event = core.loadExtScript("ext:deno_web/02_event.js");
const fetch = core.loadExtScript("ext:deno_fetch/26_fetch.js");
const caches = core.loadExtScript("ext:deno_cache/01_cache.js");
const file = core.loadExtScript("ext:deno_web/09_file.js");
const fileReader = core.loadExtScript("ext:deno_web/10_filereader.js");
const formData = core.loadExtScript("ext:deno_fetch/21_formdata.js");
const headers = core.loadExtScript("ext:deno_fetch/20_headers.js");
const streams = core.loadExtScript("ext:deno_web/06_streams.js");
const streams2 = core.loadExtScript("ext:deno_web/14_compression.js");
const timers = core.loadExtScript("ext:deno_web/02_timers.js");
const url = core.loadExtScript("ext:deno_web/00_url.js");
const urlPattern = core.loadExtScript("ext:deno_web/01_urlpattern.js");
import * as webSocket from "ext:deno_websocket/01_websocket.js";
const response = core.loadExtScript("ext:deno_fetch/23_response.js");
const request = core.loadExtScript("ext:deno_fetch/23_request.js");
const globalInterfaces = core.loadExtScript(
  "ext:deno_web/04_global_interfaces.js",
);
core.loadExtScript("ext:deno_web/16_image_data.js");
core.loadExtScript("ext:deno_web/01_broadcast_channel.js");
const performance = core.loadExtScript("ext:deno_web/15_performance.js");
const messagePort = core.loadExtScript("ext:deno_web/13_message_port.js");
import "ext:deno_websocket/02_websocketstream.js";
core.loadExtScript("ext:deno_fetch/27_eventsource.js");
core.loadExtScript("ext:deno_webgpu/00_init.js");
core.loadExtScript("ext:deno_canvas/02_surface.js");

import "ext:ai/onnxruntime/cache_adapter.js";
import {
  installWasmMemoryTracking,
  startWasmMemoryPolling,
} from "ext:runtime/wasm_memory_tracker.js";

import { FLOW_ENV } from "ext:env/env.js";

import { FlowEventListener } from "ext:user_event_worker/event_worker.js";
import {
  installEdgeRuntimeNamespace,
  installFlowNamespace,
  installTrexNamespace,
} from "ext:runtime/namespaces.js";

import "ext:runtime/promises.js";
import { installPromiseHook } from "ext:runtime/async_hook.js";
import { registerErrors } from "ext:runtime/errors.js";
import { denoOverrides, fsVars } from "ext:runtime/denoOverrides.js";
import { installTrexasUpgradeHttpRaw } from "ext:runtime/http.js";
import { registerDeclarativeServer } from "ext:runtime/00_serve.js";
const { bootstrap: bootstrapOtel } = core.loadExtScript(
  "ext:deno_telemetry/telemetry.ts",
);

import {
  formatException,
  getterOnly,
  nonEnumerable,
  readOnly,
  writable,
} from "ext:runtime/fieldUtils.js";

import {
  Navigator,
  navigator,
  setLanguage,
  setNumCpus,
  setUserAgent,
} from "ext:runtime/navigator.js";

// flow(2.9.0): `00_webidl.js` is a `lazy_loaded_js` script, not an ESM module,
// so it must be pulled via `core.loadExtScript` (matching deno's own usage,
// e.g. ext/web/06_streams.js) rather than a static `import`.
const webidl = core.loadExtScript("ext:deno_webidl/00_webidl.js");

let globalThis_;

const ops = core.ops;
const v8Console = globalThis.console;

const {
  Error,
  ArrayPrototypePop,
  ArrayPrototypeShift,
  ObjectAssign,
  ObjectKeys,
  ArrayPrototypePush,
  ObjectDefineProperty,
  ObjectDefineProperties,
  ObjectSetPrototypeOf,
  ObjectHasOwn,
  SafeSet,
  StringPrototypeIncludes,
  StringPrototypeSplit,
  StringPrototypeTrim,
  Symbol,
} = primordials;

// Set up Deno.internal Symbol for Node.js compatibility
const internalSymbol = Symbol("Deno.internal");
denoOverrides.internal = internalSymbol;
denoOverrides[internalSymbol] = internals;

let image;
function ImageNonEnumerable(getter) {
  let valueIsSet = false;
  let value;

  return {
    get() {
      loadImage();

      if (valueIsSet) {
        return value;
      } else {
        return getter();
      }
    },
    set(v) {
      loadImage();

      valueIsSet = true;
      value = v;
    },
    enumerable: false,
    configurable: true,
  };
}
function ImageWritable(getter) {
  let valueIsSet = false;
  let value;

  return {
    get() {
      loadImage();

      if (valueIsSet) {
        return value;
      } else {
        return getter();
      }
    },
    set(v) {
      loadImage();

      valueIsSet = true;
      value = v;
    },
    enumerable: true,
    configurable: true,
  };
}
function loadImage() {
  if (!image) {
    image = ops.op_lazy_load_esm("ext:deno_image/01_image.js");
  }
}

const globalScope = {
  console: nonEnumerable(
    new console.Console((msg, level) => core.print(msg, level > 1)),
  ),

  // cache api
  caches: nonEnumerable(caches.cacheStorage()),
  // timers
  clearInterval: writable(timers.clearInterval),
  clearTimeout: writable(timers.clearTimeout),
  setInterval: writable(timers.setInterval),
  setTimeout: writable(timers.setTimeout),

  // fetch
  Request: nonEnumerable(request.Request),
  Response: nonEnumerable(response.Response),
  Headers: nonEnumerable(headers.Headers),
  fetch: writable(fetch.fetch),

  // base64
  atob: writable(base64.atob),
  btoa: writable(base64.btoa),

  // encoding
  TextDecoder: nonEnumerable(encoding.TextDecoder),
  TextEncoder: nonEnumerable(encoding.TextEncoder),
  TextDecoderStream: nonEnumerable(encoding.TextDecoderStream),
  TextEncoderStream: nonEnumerable(encoding.TextEncoderStream),

  // url
  URL: nonEnumerable(url.URL),
  URLPattern: nonEnumerable(urlPattern.URLPattern),
  URLSearchParams: nonEnumerable(url.URLSearchParams),

  // crypto
  CryptoKey: nonEnumerable(crypto.CryptoKey),
  // Lazy getter (mirrors upstream 98_global_scope_shared.js): reading
  // `crypto.crypto` mints the cppgc-backed Crypto/SubtleCrypto singletons,
  // and the cppgc heap is not attached at snapshot-build time.
  crypto: getterOnly(() => crypto.crypto),
  Crypto: nonEnumerable(crypto.Crypto),
  SubtleCrypto: nonEnumerable(crypto.SubtleCrypto),

  // streams
  ByteLengthQueuingStrategy: nonEnumerable(
    streams.ByteLengthQueuingStrategy,
  ),
  CountQueuingStrategy: nonEnumerable(
    streams.CountQueuingStrategy,
  ),
  ReadableStream: nonEnumerable(streams.ReadableStream),
  ReadableStreamDefaultReader: nonEnumerable(
    streams.ReadableStreamDefaultReader,
  ),
  ReadableByteStreamController: nonEnumerable(
    streams.ReadableByteStreamController,
  ),
  ReadableStreamBYOBReader: nonEnumerable(
    streams.ReadableStreamBYOBReader,
  ),
  ReadableStreamBYOBRequest: nonEnumerable(
    streams.ReadableStreamBYOBRequest,
  ),
  ReadableStreamDefaultController: nonEnumerable(
    streams.ReadableStreamDefaultController,
  ),
  TransformStream: nonEnumerable(streams.TransformStream),
  TransformStreamDefaultController: nonEnumerable(
    streams.TransformStreamDefaultController,
  ),
  WritableStream: nonEnumerable(streams.WritableStream),
  WritableStreamDefaultWriter: nonEnumerable(
    streams.WritableStreamDefaultWriter,
  ),
  WritableStreamDefaultController: nonEnumerable(
    streams.WritableStreamDefaultController,
  ),
  CompressionStream: nonEnumerable(
    streams2.CompressionStream,
  ),
  DecompressionStream: nonEnumerable(
    streams2.DecompressionStream,
  ),
  // event
  CloseEvent: nonEnumerable(event.CloseEvent),
  CustomEvent: nonEnumerable(event.CustomEvent),
  ErrorEvent: nonEnumerable(event.ErrorEvent),
  Event: nonEnumerable(event.Event),
  EventTarget: nonEnumerable(event.EventTarget),
  MessageEvent: nonEnumerable(event.MessageEvent),
  PromiseRejectionEvent: nonEnumerable(event.PromiseRejectionEvent),
  ProgressEvent: nonEnumerable(event.ProgressEvent),
  reportError: writable(event.reportError),
  DOMException: nonEnumerable(DOMException),

  // file
  Blob: nonEnumerable(file.Blob),
  File: nonEnumerable(file.File),
  FileReader: nonEnumerable(fileReader.FileReader),

  // form data
  FormData: nonEnumerable(formData.FormData),

  // abort signal
  AbortController: nonEnumerable(abortSignal.AbortController),
  AbortSignal: nonEnumerable(abortSignal.AbortSignal),

  // Image
  ImageData: ImageNonEnumerable(() => image.ImageData),
  ImageBitmap: ImageNonEnumerable(() => image.ImageBitmap),
  createImageBitmap: ImageWritable(() => image.createImageBitmap),

  // web sockets
  WebSocket: nonEnumerable(webSocket.WebSocket),

  // performance
  Performance: nonEnumerable(performance.Performance),
  PerformanceEntry: nonEnumerable(performance.PerformanceEntry),
  PerformanceMark: nonEnumerable(performance.PerformanceMark),
  PerformanceMeasure: nonEnumerable(performance.PerformanceMeasure),
  performance: writable(performance.performance),

  // messagePort
  MessageChannel: nonEnumerable(messagePort.MessageChannel),
  structuredClone: writable(messagePort.structuredClone),

  // node globals: lazy - first access pulls the node:* polyfill, which
  // self-bootstraps from internals.__nodeBootstrapArgs
  process: core.propWritableLazyLoaded((m) => m.default, lazyProcessMod),
  Buffer: core.propWritableLazyLoaded((m) => m.Buffer, lazyBufferMod),
  setImmediate: core.propWritableLazyLoaded(
    (m) => m.setImmediate,
    lazyNodeTimersMod,
  ),
  clearImmediate: core.propWritableLazyLoaded(
    (m) => m.clearImmediate,
    lazyNodeTimersMod,
  ),

  // Branding as a WebIDL object
  [webidl.brand]: nonEnumerable(webidl.brand),
};

let verboseDeprecatedApiWarning = false;
let deprecatedApiWarningDisabled = false;
const ALREADY_WARNED_DEPRECATED = new SafeSet();

function warnOnDeprecatedApi(apiName, stack, ...suggestions) {
  if (deprecatedApiWarningDisabled) {
    return;
  }

  if (!verboseDeprecatedApiWarning) {
    if (ALREADY_WARNED_DEPRECATED.has(apiName)) {
      return;
    }
    ALREADY_WARNED_DEPRECATED.add(apiName);
    globalThis.console.error(
      `%cwarning: %cUse of deprecated "${apiName}" API. This API will be removed in Deno 2. Run again with DENO_VERBOSE_WARNINGS=1 to get more details.`,
      "color: yellow;",
      "font-weight: bold;",
    );
    return;
  }

  if (ALREADY_WARNED_DEPRECATED.has(apiName + stack)) {
    return;
  }

  // If we haven't warned yet, let's do some processing of the stack trace
  // to make it more useful.
  const stackLines = StringPrototypeSplit(stack, "\n");
  ArrayPrototypeShift(stackLines);
  while (stackLines.length > 0) {
    // Filter out internal frames at the top of the stack - they are not useful
    // to the user.
    if (
      StringPrototypeIncludes(stackLines[0], "(ext:") ||
      StringPrototypeIncludes(stackLines[0], "(node:") ||
      StringPrototypeIncludes(stackLines[0], "<anonymous>")
    ) {
      ArrayPrototypeShift(stackLines);
    } else {
      break;
    }
  }
  // Now remove the last frame if it's coming from "ext:core" - this is most likely
  // event loop tick or promise handler calling a user function - again not
  // useful to the user.
  if (
    stackLines.length > 0 &&
    StringPrototypeIncludes(stackLines[stackLines.length - 1], "(ext:core/")
  ) {
    ArrayPrototypePop(stackLines);
  }

  let isFromRemoteDependency = false;
  const firstStackLine = stackLines[0];
  if (firstStackLine && !StringPrototypeIncludes(firstStackLine, "file:")) {
    isFromRemoteDependency = true;
  }

  ALREADY_WARNED_DEPRECATED.add(apiName + stack);
  globalThis.console.error(
    `%cwarning: %cUse of deprecated "${apiName}" API. This API will be removed in Deno 2.`,
    "color: yellow;",
    "font-weight: bold;",
  );

  globalThis.console.error();
  globalThis.console.error(
    "See the Deno 1 to 2 Migration Guide for more information at https://docs.deno.com/runtime/manual/advanced/migrate_deprecations",
  );
  globalThis.console.error();
  if (stackLines.length > 0) {
    globalThis.console.error("Stack trace:");
    for (let i = 0; i < stackLines.length; i++) {
      globalThis.console.error(`  ${StringPrototypeTrim(stackLines[i])}`);
    }
    globalThis.console.error();
  }

  for (let i = 0; i < suggestions.length; i++) {
    const suggestion = suggestions[i];
    globalThis.console.error(
      `%chint: ${suggestion}`,
      "font-weight: bold;",
    );
  }

  if (isFromRemoteDependency) {
    globalThis.console.error(
      `%chint: It appears this API is used by a remote dependency. Try upgrading to the latest version of that dependency.`,
      "font-weight: bold;",
    );
  }
  globalThis.console.error();
}
ObjectAssign(internals, { warnOnDeprecatedApi });

function runtimeStart(target) {
  // core.setMacrotaskCallback(timers.handleTimerMacrotask);
  // core.setMacrotaskCallback(promiseRejectMacrotaskCallback);

  core.setWasmStreamingCallback(fetch.handleWasmStreaming);
  ops.op_set_format_exception_callback(formatException);
  core.setBuildInfo(target);

  // deno-lint-ignore prefer-primordials
  Error.prepareStackTrace = core.prepareStackTrace;

  registerErrors();
}

// We need to delete globalThis.console
// Before setting up a new one
// This is because v8 sets a console that can't be easily overriden
// and collides with globalScope.console
delete globalThis.console;
ObjectDefineProperties(globalThis, globalScope);

const globalProperties = {
  Window: globalInterfaces.windowConstructorDescriptor,
  window: getterOnly(() => globalThis),
  Navigator: nonEnumerable(Navigator),
  navigator: getterOnly(() => navigator),
  self: getterOnly(() => globalThis),
};
ObjectDefineProperties(globalThis, globalProperties);

const MAKE_HARD_ERR_FN = (msg) => {
  return () => {
    throw new globalThis_.Deno.errors.PermissionDenied(msg);
  };
};

const DENIED_DENO_FS_API_LIST = ObjectKeys(fsVars)
  .reduce(
    (acc, it) => {
      if (fsVars[it] !== void 0) {
        acc[it] = MAKE_HARD_ERR_FN(`Deno.${it} is blocklisted`);
      }
      return acc;
    },
    {},
  );

function dispatchLoadEvent() {
  globalThis_.dispatchEvent(new Event("load"));
}

function dispatchBeforeUnloadEvent(reason) {
  globalThis_.dispatchEvent(
    new CustomEvent("beforeunload", {
      cancelable: true,
      detail: { reason: reason ?? null },
    }),
  );
}

function dispatchUnloadEvent() {
  globalThis_.dispatchEvent(new Event("unload"));
}

function dispatchDrainEvent() {
  internals.drain = true;
  globalThis_.dispatchEvent(new Event("drain"));
}

// Notification that the core received an unhandled promise rejection that is about to
// terminate the runtime. If we can handle it, attempt to do so.
function processUnhandledPromiseRejection(promise, reason) {
  const rejectionEvent = new event.PromiseRejectionEvent(
    "unhandledrejection",
    {
      cancelable: true,
      promise,
      reason,
    },
  );

  // Note that the handler may throw, causing a recursive "error" event
  globalThis_.dispatchEvent(rejectionEvent);

  // If event was not yet prevented, try handing it off to Node compat layer
  // (if it was initialized)
  if (
    !rejectionEvent.defaultPrevented &&
    typeof internals.nodeProcessUnhandledRejectionCallback !== "undefined"
  ) {
    internals.nodeProcessUnhandledRejectionCallback(rejectionEvent);
  }

  // If event was not prevented (or "unhandledrejection" listeners didn't
  // throw) we will let Rust side handle it.
  if (rejectionEvent.defaultPrevented) {
    return true;
  }

  return false;
}

function processRejectionHandled(promise, reason) {
  const rejectionHandledEvent = new event.PromiseRejectionEvent(
    "rejectionhandled",
    { promise, reason },
  );

  // Note that the handler may throw, causing a recursive "error" event
  globalThis_.dispatchEvent(rejectionHandledEvent);

  if (typeof internals.nodeProcessRejectionHandledCallback !== "undefined") {
    internals.nodeProcessRejectionHandledCallback(rejectionHandledEvent);
  }
}

globalThis.bootstrapSBEdge = (opts, ctx) => {
  let bootstrapMockFnThrowError = false;

  // Replace upstream upgradeHttpRaw with trexas's fence-based variant.
  // Must run here (not at http.js module-load time) because deno_http's
  // 00_serve.ts loads non-deterministically relative to runtime/http.js,
  // and 00_serve.ts unconditionally writes internals.upgradeHttpRaw on load.
  installTrexasUpgradeHttpRaw();

  globalThis_ = globalThis;

  // We should delete this after initialization,
  // Deleting it during bootstrapping can backfire
  delete globalThis.__bootstrap;
  delete globalThis.bootstrap;

  ObjectSetPrototypeOf(globalThis, Window.prototype);
  event.setEventTargetData(globalThis);
  event.saveGlobalThisReference(globalThis);

  const eventHandlers = [
    "error",
    "load",
    "beforeunload",
    "unload",
    "unhandledrejection",
    "drain",
  ];

  eventHandlers.forEach((handlerName) =>
    event.defineEventHandler(globalThis, handlerName)
  );

  // Nothing listens to this, but it warms up the code paths for event dispatch
  (new event.EventTarget()).dispatchEvent(new Event("warmup"));

  /**
   * @type {{
   * target: string,
   * kind: 'user' | 'main' | 'event',
   * inspector: boolean,
   * migrated: boolean,
   * debug: boolean,
   * version: {
   * 	runtime: string,
   * 	deno: string,
   * },
   * flags: {
   * 	SHOULD_DISABLE_DEPRECATED_API_WARNING: boolean,
   * 	SHOULD_USE_VERBOSE_DEPRECATED_API_WARNING: boolean
   * },
   * otel: [] | [number, number]
   * }}
   */
  const {
    migrated,
    target,
    kind,
    version,
    inspector,
    flags,
    otel,
  } = opts;

  deprecatedApiWarningDisabled = flags["SHOULD_DISABLE_DEPRECATED_API_WARNING"];
  verboseDeprecatedApiWarning =
    flags["SHOULD_USE_VERBOSE_DEPRECATED_API_WARNING"];
  bootstrapMockFnThrowError = ctx?.shouldBootstrapMockFnThrowError ?? false;

  runtimeStart(target);

  ObjectAssign(internals, {
    bootstrapArgs: { opts },
    worker: { kind },
    __ctx: ctx,
  });

  installPromiseHook(kind);
  installEdgeRuntimeNamespace(kind, ctx.terminationRequestToken);
  installTrexNamespace(kind, ctx.terminationRequestToken);
  installFlowNamespace(kind);

  ObjectDefineProperty(
    globalThis,
    "FLOW_VERSION",
    readOnly(String(version.runtime)),
  );
  ObjectDefineProperty(globalThis, "DENO_VERSION", readOnly(version.deno));

  // set these overrides after runtimeStart
  ObjectDefineProperties(denoOverrides, {
    build: readOnly(core.build),
    env: readOnly(FLOW_ENV),
    pid: readOnly(globalThis.__pid),
    args: readOnly([]), // args are set to be empty
    mainModule: getterOnly(() => ops.op_main_module()),
    version: getterOnly(() => ({
      deno:
        // TODO(flow): change to a well-known name for the ecosystem.
        `flow-edge-runtime-${globalThis.FLOW_VERSION} (compatible with Deno v${globalThis.DENO_VERSION})`,
      v8: "11.6.189.12",
      typescript: "5.1.6",
    })),
  });

  if (kind === "user") {
    ObjectDefineProperties(globalThis, {
      console: nonEnumerable(
        new console.Console((msg, level) => {
          try {
            ops.op_user_worker_log(msg, level);
          } catch {
            // ignore
          }
          if (inspector) {
            const method = level === 0
              ? "debug"
              : level === 1
              ? "log"
              : level === 2
              ? "warn"
              : "error";
            v8Console[method](msg.trimEnd());
          }
        }),
      ),
    });

    // flow: expose the duplex MessagePort back to the host (main isolate). The
    // pair is created in Rust by op_user_worker_create; this worker's half was
    // installed into op_state and its rid is handed over here. Surfaced as
    // `EdgeRuntime.parentPort` (postMessage/onmessage, structured clone).
    const parentPortRid = ops.op_flow_parent_port_rid();
    if (parentPortRid >= 0) {
      const { createMessagePort } = core.loadExtScript(
        "ext:deno_web/13_message_port.js",
      );
      globalThis.EdgeRuntime.parentPort = createMessagePort(parentPortRid);
      globalThis.EdgeRuntime.parentPorts = [globalThis.EdgeRuntime.parentPort];

      // Accept ADDITIONAL parent ports, delivered when a later host-side
      // `EdgeRuntime.userWorkers.create()` resolves to this already-running
      // worker (pool reuse) - each such create() gets its own duplex channel,
      // SharedWorker-style. New ports are appended to
      // `EdgeRuntime.parentPorts` and handed to `EdgeRuntime.onparentport`
      // when set (messages queue inside the port until a handler attaches).
      // The pending accept op is unref'd so it never holds the event loop.
      (async () => {
        while (true) {
          const promise = ops.op_flow_recv_parent_port();
          core.unrefOpPromise(promise);
          const rid = await promise;
          if (rid < 0) {
            break;
          }
          const port = createMessagePort(rid);
          ArrayPrototypePush(globalThis.EdgeRuntime.parentPorts, port);
          const handler = globalThis.EdgeRuntime.onparentport;
          if (typeof handler === "function") {
            try {
              handler(port);
            } catch (error) {
              globalThis.console.error(
                "EdgeRuntime.onparentport threw:",
                error,
              );
            }
          }
        }
      })();
    }
  } else if (inspector) {
    ObjectDefineProperties(globalThis, {
      console: nonEnumerable(v8Console),
    });
  }

  bootstrapOtel(otel);

  ObjectDefineProperty(globalThis, "Deno", readOnly(denoOverrides));

  setNumCpus(1); // explicitly setting no of CPUs to 1 (since we don't allow workers)
  setUserAgent(
    // TODO(flow): change to a well-known name for the ecosystem.
    `Deno/${globalThis.DENO_VERSION} (variant; FlowEdgeRuntime/${globalThis.FLOW_VERSION})`,
  );
  setLanguage("en");

  core.addMainModuleHandler((main) => {
    if (migrated && !ctx?.suppressEszipMigrationWarning) {
      globalThis.console.warn(
        "It appears this function was deployed using an older version of Flow CLI.\n",
        "For best performance and compatibility we recommend re-deploying the function using the latest version of the CLI.",
      );
    }

    // Find declarative fetch handler
    if (ObjectHasOwn(main, "default")) {
      registerDeclarativeServer(main.default);
    }
  });

  /// DISABLE SHARED MEMORY AND INSTALL MEM CHECK TIMING

  // NOTE: We should not allow user workers to use shared memory. This is because they are not
  // counted in the external memory statistics of the individual isolates.

  // NOTE(Nyannyacha): Put below inside `kind === 'user'` block if we have the plan to support a
  // shared array buffer across the isolates. But for now, we explicitly disabled the shared
  // buffer option between isolate globally in `deno_runtime.rs`, so this patch also applies
  // regardless of worker type.
  // Wrap WebAssembly.Memory/Instance for MemCheck tracking FIRST, so the
  // shared-memory denial patch below captures (and layers on top of) the
  // tracking wrapper. Deferred to here because the module evaluates during
  // snapshot creation, where `globalThis.WebAssembly` does not exist.
  installWasmMemoryTracking();

  const wasmMemoryCtor = globalThis.WebAssembly.Memory;
  const wasmMemoryPrototypeGrow = wasmMemoryCtor.prototype.grow;

  function patchedWasmMemoryPrototypeGrow(delta) {
    const mem = wasmMemoryPrototypeGrow.call(this, delta);

    ops.op_schedule_mem_check();

    return mem;
  }

  wasmMemoryCtor.prototype.grow = patchedWasmMemoryPrototypeGrow;

  function patchedWasmMemoryCtor(maybeOpts) {
    if (typeof maybeOpts === "object" && maybeOpts["shared"] === true) {
      throw new TypeError("Creating a shared memory is not supported");
    }

    return new wasmMemoryCtor(maybeOpts);
  }

  globalThis.SharedArrayBuffer = globalThis.ArrayBuffer;
  globalThis.WebAssembly.Memory = patchedWasmMemoryCtor;

  /// DISABLE SHARED MEMORY INSTALL MEM CHECK TIMING

  if (kind === "user") {
    const apisToBeOverridden = ctx?.allowHostFsAccess ? {} : {
      ...DENIED_DENO_FS_API_LIST,

      "cwd": true,

      "open": true,
      "lstat": true,
      "stat": true,
      "realPath": true,
      "realPathSync": true,
      "create": true,
      "remove": true,
      "writeFile": true,
      "writeTextFile": true,
      "readFile": true,
      "readTextFile": true,
      "mkdir": true,
      "makeTempDir": true,
      "makeTempFile": true,
      "readDir": true,

      "kill": "mock",
      "exit": "mock",
      "addSignalListener": "mock",
      "removeSignalListener": "mock",

      "lstatSync": true,
      "statSync": true,
      "removeSync": true,
      "writeFileSync": true,
      "writeTextFileSync": true,
      "readFileSync": true,
      "readTextFileSync": true,
      "mkdirSync": true,
      "makeTempDirSync": true,
      "makeTempFileSync": true,
      "readDirSync": true,

      // TODO(flow): use a non-hardcoded path
      "execPath": () => "/bin/trex",
      "memoryUsage": () => ops.op_runtime_memory_usage(),
    };

    if (ctx?.useReadSyncFileAPI) {
      // Only override if allowHostFsAccess is not explicitly enabled
      if (!ctx?.allowHostFsAccess) {
        apisToBeOverridden["readFileSync"] = "warnIfRuntimeIsAlreadyInit";
        apisToBeOverridden["readTextFileSync"] = "warnIfRuntimeIsAlreadyInit";
      }
    }

    const apiNames = ObjectKeys(apisToBeOverridden);

    for (const name of apiNames) {
      const value = apisToBeOverridden[name];

      if (value === false) {
        delete Deno[name];
      } else if (value === true) {
        // Allow the API - do nothing, keep original function
        continue;
      } else if (typeof value === "function") {
        Deno[name] = value;
      } else if (typeof value === "string") {
        switch (value) {
          case "mock": {
            Deno[name] = () => {
              if (bootstrapMockFnThrowError) {
                throw new TypeError("called MOCK_FN");
              }
            };
            break;
          }
          case "allowIfRuntimeIsInInit": {
            const originalFn = Deno[name];
            const blocklistedFn = MAKE_HARD_ERR_FN(
              `Deno.${name} is blocklisted on the current context`,
            );
            Deno[name] = (...args) => {
              if (ops.op_is_runtime_init()) {
                return originalFn(...args);
              } else {
                return blocklistedFn();
              }
            };
            break;
          }
          case "warnIfRuntimeIsAlreadyInit": {
            const originalFn = Deno[name];
            Deno[name] = (...args) => {
              if (ops.op_is_runtime_init()) {
                return originalFn(...args);
              } else {
                globalThis.console.error(
                  `WARNING: Do not use Deno.${name} inside the async callback. This has performance impacts and will be disallowed in the future.\nUse the async version instead.`,
                );
                return originalFn(...args);
              }
            };
            break;
          }
        }
      }
    }
  }

  if (kind === "event") {
    ObjectDefineProperties(globalThis, {
      EventManager: getterOnly(() => FlowEventListener),
    });
  }

  // flow(2.9.0 node-compat): node:module (01_require.js) is lazy_loaded_esm,
  // so `globalThis.nodeBootstrap` is NOT defined at bootstrap time (deno's
  // node-defer model, see runtime/js/99_main.js). Stash the bootstrap args:
  // node:process and 01_require.js self-bootstrap from them when the first
  // `node:*` module loads (triggered by user code importing node builtins or
  // touching the lazy process/Buffer globals). Each worker acts as its own
  // main thread, matching edge's pre-2.9.0 `runningOnMainThread: true`.
  internals.__nodeBootstrapArgs = {
    usesLocalNodeModulesDir: false,
    runningOnMainThread: true,
    argv0: "flow",
    nodeDebug: Deno.env.get("NODE_DEBUG") ?? "",
    denoArgs: [],
    denoVersion: Deno.version,
  };

  startWasmMemoryPolling();

  delete globalThis.bootstrapSBEdge;
};

globalThis.bootstrap = {
  dispatchLoadEvent,
  dispatchUnloadEvent,
  dispatchBeforeUnloadEvent,
  // dispatchProcessExitEvent,
  // dispatchProcessBeforeExitEvent,
  dispatchDrainEvent,
};

core.setUnhandledPromiseRejectionHandler(processUnhandledPromiseRejection);
core.setHandledPromiseRejectionHandler(processRejectionHandled);
