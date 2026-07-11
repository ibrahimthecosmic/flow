// A deep node of the service subtree. entry_deep.js imports it DIRECTLY (not
// through mod.ts) to exercise the deep-import behavior: it is bundled under
// root-form exclusion (reachable via a non-excluded path) but excluded under
// glob-form exclusion (matches the subtree pattern).
export const util = "deep-util";
