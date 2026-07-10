// Reports whether host filesystem access works in this worker.
try {
  const content = await Deno.readTextFile("/etc/hostname");
  console.log(`host-fs-access ok: ${content.trim().length >= 0}`);
} catch (e) {
  console.log(`host-fs-access denied: ${e.toString()}`);
}
