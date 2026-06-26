// named + default + namespace imports
import def, { a, b as bb } from "./_helpers/named.mjs";
import * as ns from "./_helpers/named.mjs";
console.log(a, bb, def.tag, Object.keys(ns).sort().join(","));
