// v8 147 no longer surfaces WasmMemoryObject through HeapStatistics, so we
// track WebAssembly.Memory buffers manually for MemCheck. Rust polls via
// globalThis.__trex_poll_wasm_bytes() and stores the total in a shared atomic.
//
// NOTE: all WebAssembly touches live inside `installWasmMemoryTracking()`,
// called from bootstrap.js at runtime. This module is evaluated during
// SNAPSHOT creation, where `globalThis.WebAssembly` does not exist (and any
// top-level wrapping would be baked-in dead state anyway).

import { core, primordials } from "ext:core/mod.js";

const {
  ArrayPrototypePush,
  ObjectDefineProperty,
  ObjectKeys,
  ObjectPrototypeIsPrototypeOf,
  WeakRefPrototypeDeref,
} = primordials;

const opSetWasmMemoryBytes = core.ops.op_set_wasm_memory_bytes;

const memories = [];
let origMemoryCtor;

function trackMemory(mem) {
  if (mem !== undefined && mem !== null) {
    ArrayPrototypePush(memories, new WeakRef(mem));
  }
}

function trackInstanceExports(instance) {
  const exports = instance?.exports;
  if (exports === undefined || exports === null) return;
  const keys = ObjectKeys(exports);
  for (let i = 0; i < keys.length; i++) {
    const v = exports[keys[i]];
    if (ObjectPrototypeIsPrototypeOf(origMemoryCtor.prototype, v)) {
      trackMemory(v);
    }
  }
}

// Wrap WebAssembly.Memory/Instance/instantiate(Streaming) so every wasm
// memory is registered for polling. Must run BEFORE bootstrap.js applies its
// shared-memory denial patch (which captures and layers on top of these).
export function installWasmMemoryTracking() {
  const Wasm = globalThis.WebAssembly;

  const OrigMemory = Wasm.Memory;
  const OrigInstance = Wasm.Instance;
  const origInstantiate = Wasm.instantiate;
  const origInstantiateStreaming = Wasm.instantiateStreaming;
  origMemoryCtor = OrigMemory;

  function WrapMemory(desc, ...rest) {
    const mem = new OrigMemory(desc, ...rest);
    trackMemory(mem);
    return mem;
  }
  WrapMemory.prototype = OrigMemory.prototype;

  function WrapInstance(mod, imports) {
    const instance = new OrigInstance(mod, imports);
    trackInstanceExports(instance);
    return instance;
  }
  WrapInstance.prototype = OrigInstance.prototype;

  ObjectDefineProperty(Wasm, "Memory", {
    configurable: true,
    writable: true,
    value: WrapMemory,
  });
  ObjectDefineProperty(Wasm, "Instance", {
    configurable: true,
    writable: true,
    value: WrapInstance,
  });

  Wasm.instantiate = function instantiate(source, imports) {
    return origInstantiate(source, imports).then((result) => {
      if (ObjectPrototypeIsPrototypeOf(OrigInstance.prototype, result)) {
        trackInstanceExports(result);
      } else if (result?.instance !== undefined) {
        trackInstanceExports(result.instance);
      }
      return result;
    });
  };

  if (origInstantiateStreaming !== undefined) {
    Wasm.instantiateStreaming = function instantiateStreaming(
      source,
      imports,
    ) {
      return origInstantiateStreaming(source, imports).then((result) => {
        if (result?.instance !== undefined) {
          trackInstanceExports(result.instance);
        }
        return result;
      });
    };
  }
}

// buffer.byteLength reflects growth from the `memory.grow` instruction too.
function pollWasmBytes() {
  let total = 0;
  let write = 0;
  for (let i = 0; i < memories.length; i++) {
    const ref = memories[i];
    const mem = WeakRefPrototypeDeref(ref);
    if (mem !== undefined) {
      total += mem.buffer.byteLength;
      memories[write++] = ref;
    }
  }
  memories.length = write;
  opSetWasmMemoryBytes(total);
}

// Called from bootstrap.js after timers are wired (op_node_new_async_id
// panics if invoked at module-load time).
export function startWasmMemoryPolling() {
  const { setInterval, unrefTimer } = core.loadExtScript(
    "ext:deno_web/02_timers.js",
  );
  // Unref so this never keeps the event loop alive on its own.
  const id = setInterval(pollWasmBytes, 100);
  unrefTimer(id);
}
