globalThis.addEventListener("beforeunload", (ev) => {
  console.log(`triggered ${(ev as CustomEvent).detail?.reason}`);
});

await new Promise((res) => setTimeout(res, 60_000));
