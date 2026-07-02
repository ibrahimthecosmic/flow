// Facade for ext:deno_os/30_os.js
//
// Upstream deno_node polyfills (process.ts, os.ts, wasi.ts) pull this
// specifier via `core.loadExtScript(...)`, so it must be a `lazy_loaded_js`
// SCRIPT (IIFE returning its exports), matching the real ext/os/30_os.js
// form. The exit helpers delegate to `ext:os/exit.js` (worker-safe exit with
// the single shared exitHandler state), bridged through `internals` because
// scripts cannot static-import ESM. Everything else is stubbed so that no
// actual host information leaks into user workers.
//
// NOTE: `process.env` does NOT flow through the `env` stub below - deno_node's
// _process/process.ts proxies `Deno.env` directly, which in edge workers is
// the per-worker env. The stub only covers direct `denoOs.env` consumers.

(function () {
const { internals } = __bootstrap;
const { exit, getExitCode, setExitCode, setExitHandler } = internals.flowOsExit;

function loadavg() {
  return [0, 0, 0];
}

function hostname() {
  return "localhost";
}

function osRelease() {
  return "0.0.0-trex";
}

function osUptime() {
  return 0;
}

function systemMemoryInfo() {
  return null;
}

function networkInterfaces() {
  return [];
}

function gid() {
  return 0;
}

function uid() {
  return 0;
}

const env = {
  get(_key) {
    return undefined;
  },
  toObject() {
    return {};
  },
  set(_key, _value) {},
  has(_key) {
    return false;
  },
  delete(_key) {},
};

function execPath() {
  return "";
}

return {
  env,
  execPath,
  exit,
  getExitCode,
  gid,
  hostname,
  loadavg,
  networkInterfaces,
  osRelease,
  osUptime,
  setExitCode,
  setExitHandler,
  systemMemoryInfo,
  uid,
};
})();
