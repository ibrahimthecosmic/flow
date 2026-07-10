globalThis.addEventListener("beforeunload", (ev) => {
  console.log(`triggered ${(ev as CustomEvent).detail?.reason}`);
});

const arr = [];
while (true) {
  arr.push(new Uint8Array(1024 * 1024));
  // yield so the event loop can deliver beforeunload
  await new Promise((res) => setTimeout(res, 10));
}
