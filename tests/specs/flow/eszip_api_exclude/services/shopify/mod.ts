// The service root. Excluded from tenant bundles and provided at runtime.
// Imports a private dep (only reachable through this subtree) and a shared dep
// (also used by the tenant).
import { shared } from "../../shared.js";
import { helper } from "./private.ts";

export const Shopify = `shopify:${shared}:${helper}`;
