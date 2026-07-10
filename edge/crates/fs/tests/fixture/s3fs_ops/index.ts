// S3 fs-mount exerciser, driven by FS_TEST_* env vars. Results are reported
// through console logs ("op <name> ok ..." / "op failed: ..."), which the
// Rust side observes via the worker events channel.

const op = Deno.env.get("FS_TEST_OP") ?? "";
const rel = Deno.env.get("FS_TEST_PATH") ?? "";
const path = `/s3/${rel}`;
const size = Number(Deno.env.get("FS_TEST_SIZE") ?? "0");
const recursive = Deno.env.get("FS_TEST_RECURSIVE") === "true";

function pattern(i: number): number {
  return (i * 31 + 7) & 0xff;
}

try {
  switch (op) {
    case "write": {
      const buf = new Uint8Array(size);
      for (let i = 0; i < size; i++) buf[i] = pattern(i);
      await Deno.writeFile(path, buf);
      console.log(`op write ok ${size}`);
      break;
    }

    case "verify": {
      const buf = await Deno.readFile(path);
      if (buf.length !== size) {
        throw new Error(`size mismatch: ${buf.length} != ${size}`);
      }
      for (let i = 0; i < buf.length; i++) {
        if (buf[i] !== pattern(i)) {
          throw new Error(`byte mismatch at offset ${i}`);
        }
      }
      console.log(`op verify ok ${size}`);
      break;
    }

    case "mkdir": {
      await Deno.mkdir(path, { recursive });
      console.log("op mkdir ok");
      break;
    }

    case "read-dir": {
      const entries = [];
      for await (const e of Deno.readDir(path)) {
        entries.push({
          name: e.name,
          isFile: e.isFile,
          isDirectory: e.isDirectory,
        });
      }
      console.log(`op read-dir ok ${JSON.stringify(entries)}`);
      break;
    }

    case "remove": {
      await Deno.remove(path, { recursive });
      console.log("op remove ok");
      break;
    }

    case "read-sync-in-async": {
      // Sync fs APIs on virtual mounts are only allowed while the runtime is
      // initializing; inside an async callback they must be rejected.
      await new Promise<void>((res, rej) => {
        setTimeout(() => {
          try {
            Deno.readFileSync(path);
            rej(new Error("expected readFileSync to be blocked"));
          } catch (e) {
            console.log(`op read-sync-in-async blocked: ${e}`);
            res();
          }
        }, 10);
      });
      break;
    }

    default:
      throw new Error(`unknown op: ${op}`);
  }
} catch (e) {
  console.log(`op failed: ${e}`);
}
