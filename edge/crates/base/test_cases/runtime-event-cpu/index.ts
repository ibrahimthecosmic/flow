function mySlowFunction(baseNumber: number) {
  const now = Date.now();
  let result = 0;
  for (let i = Math.pow(baseNumber, 7); i >= 0; i--) {
    result += Math.atan(i) * Math.tan(i);
  }
  return { result, duration: Date.now() - now };
}

globalThis.addEventListener("beforeunload", (ev) => {
  console.log(`triggered ${(ev as CustomEvent).detail?.reason}`);
});

mySlowFunction(11);

await new Promise((res) => setTimeout(res, 5_000));
