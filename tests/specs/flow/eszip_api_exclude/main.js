// Exercises `FlowRuntime.bundle`'s `exclude` option: excluded module subtrees
// are left out of the archive (bare imports for runtime resolution), while a
// dependency also reachable from a non-excluded module stays bundled. Prints
// "ALL TESTS PASSED" on success.
//
// The service root is imported here with a relative specifier to keep the
// fixture hermetic; the matcher keys on the authored specifier string, so an
// import-mapped or package-imports specifier (e.g. `#services/shopify/mod.ts`)
// is excluded identically once it resolves.

function assert(cond, msg) {
  if (!cond) {
    throw new Error(`assertion failed: ${msg}`);
  }
}

async function collect(stream) {
  const chunks = [];
  let total = 0;
  for await (const chunk of stream) {
    chunks.push(chunk);
    total += chunk.byteLength;
  }
  const out = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return out;
}

// Bundle `entry` and return the set of module keys the eszip contains, plus
// their decoded contents.
async function bundleModules(entry, options) {
  const bytes = await collect(FlowRuntime.bundle(entry, options));
  assert(bytes.byteLength > 0, `bundle of ${entry} produced bytes`);
  const keys = new Set();
  const text = new Map();
  const pending = [];
  const job = FlowRuntime.unbundle(bytes);
  job.on("file", (meta, stream) => {
    if (meta.kind !== "module") return;
    keys.add(meta.path);
    pending.push(
      collect(stream).then((b) =>
        text.set(meta.path, new TextDecoder().decode(b))
      ),
    );
  });
  await job.done;
  await Promise.all(pending);
  return { keys, text };
}

const SERVICE = "./services/shopify/mod.ts";

// --- Baseline: no exclusion bundles the whole service subtree. ---------------
{
  const { keys } = await bundleModules("entry.js", {});
  assert(keys.has("entry.js"), "baseline: entrypoint bundled");
  assert(keys.has("tenant.js"), "baseline: tenant bundled");
  assert(keys.has("shared.js"), "baseline: shared bundled");
  assert(
    keys.has("services/shopify/mod.ts"),
    `baseline: service root bundled (got: ${[...keys].sort()})`,
  );
  assert(
    keys.has("services/shopify/private.ts"),
    "baseline: service-private dep bundled",
  );
}

// --- Root form: exclude the service by its authored specifier. ---------------
// Its private dep is pruned (reachable only through it); the shared dep stays
// because the tenant also imports it.
{
  const { keys, text } = await bundleModules("entry.js", {
    exclude: [SERVICE],
  });
  assert(keys.has("entry.js"), "root form: entrypoint bundled");
  assert(keys.has("tenant.js"), "root form: tenant bundled");
  assert(
    keys.has("shared.js"),
    "root form: shared dep stays bundled (also used by tenant)",
  );
  assert(
    !keys.has("services/shopify/mod.ts"),
    `root form: excluded root is NOT bundled (got: ${[...keys].sort()})`,
  );
  assert(
    !keys.has("services/shopify/private.ts"),
    "root form: service-private dep is pruned",
  );
  // The excluded import is left bare in the emitted entry source.
  assert(
    text.get("entry.js").includes(SERVICE),
    "root form: excluded import left bare in entry source",
  );
}

// --- Reachability: a deep node imported DIRECTLY by the entry stays bundled
// under root-form exclusion (it is reachable via a non-excluded path). --------
{
  const { keys } = await bundleModules("entry_deep.js", {
    exclude: [SERVICE],
  });
  assert(
    !keys.has("services/shopify/mod.ts"),
    "reachability: root excluded",
  );
  assert(
    keys.has("services/shopify/internal/util.ts"),
    "reachability: deep node imported directly stays bundled under root-form exclusion",
  );
  assert(
    !keys.has("services/shopify/private.ts"),
    "reachability: dep reachable only via the excluded root is pruned",
  );
}

// --- Glob form: exclude the whole subtree, including the deep direct import. --
{
  const { keys } = await bundleModules("entry_deep.js", {
    exclude: ["services/shopify/**"],
  });
  assert(keys.has("entry_deep.js"), "glob form: entrypoint bundled");
  assert(keys.has("shared.js"), "glob form: shared dep stays bundled");
  assert(
    !keys.has("services/shopify/mod.ts"),
    "glob form: subtree root excluded",
  );
  assert(
    !keys.has("services/shopify/private.ts"),
    "glob form: private dep excluded",
  );
  assert(
    !keys.has("services/shopify/internal/util.ts"),
    "glob form: deep node excluded even when imported directly",
  );
}

console.log("ALL TESTS PASSED");
