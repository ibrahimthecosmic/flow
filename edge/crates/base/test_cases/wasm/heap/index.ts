import init, { big_meow } from "../shared/index.ts";

// NOTE: Just defined to prevent the JsRuntime leave from the event loop
setInterval(() => {/* do nothing */}, 1000);

init();
let large_str = big_meow();
console.log(large_str.length); // to prevent optimization
