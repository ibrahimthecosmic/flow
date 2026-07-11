import { Shopify } from "./services/shopify/mod.ts";
// Deep DIRECT import into the service subtree, bypassing the mod.ts root.
import { util } from "./services/shopify/internal/util.ts";
import { tenant } from "./tenant.js";

console.log(Shopify, util, tenant);
