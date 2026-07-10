import file from "./version.json" with { type: "json" };

if (file.version !== "1.0.0") {
  throw new Error(`unexpected version: ${file.version}`);
}

console.log("json_import test passed");
