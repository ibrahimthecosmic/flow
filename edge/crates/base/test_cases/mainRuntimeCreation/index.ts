// Verifies FlowRuntime global with userWorkers API is available

if (typeof FlowRuntime === 'undefined') {
  throw new Error('FlowRuntime is not defined in main runtime');
}

if (typeof FlowRuntime !== 'object' || FlowRuntime === null) {
  throw new Error(`Expected FlowRuntime to be an object, got: ${typeof FlowRuntime}`);
}

if (!FlowRuntime.userWorkers) {
  throw new Error('FlowRuntime.userWorkers is not defined in main runtime');
}

console.log('mainRuntimeCreation test passed');
console.log('FlowRuntime keys:', Object.keys(FlowRuntime));
