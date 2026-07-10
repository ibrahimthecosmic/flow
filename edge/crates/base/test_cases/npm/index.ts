import isEven from "npm:is-even";

import { hello } from "./hello.js";
import { numbers } from "./folder1/folder2/numbers.ts";

const result = JSON.stringify({ is_even: isEven(10), hello, numbers });
const expected = '{"is_even":true,"hello":"","numbers":{"Uno":1,"Dos":2}}';

if (result !== expected) {
  throw new Error(`unexpected result: ${result}`);
}

console.log("npm test passed");
