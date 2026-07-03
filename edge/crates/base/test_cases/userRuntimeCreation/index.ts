// Verifies FlowRuntime with limited APIs (only waitUntil) is available

if (typeof FlowRuntime === 'undefined') {
  throw new Error('FlowRuntime is not defined in user runtime');
}

if (typeof FlowRuntime !== 'object' || FlowRuntime === null) {
  throw new Error(`Expected FlowRuntime to be an object, got: ${typeof FlowRuntime}`);
}

if (typeof FlowRuntime.waitUntil !== 'function') {
  throw new Error('FlowRuntime.waitUntil is not a function in user runtime');
}

if (FlowRuntime.userWorkers !== undefined) {
  throw new Error('FlowRuntime.userWorkers should not be defined in user runtime');
}

console.log('userRuntimeCreation test passed');
console.log('FlowRuntime keys:', Object.keys(FlowRuntime));
