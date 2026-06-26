import { EOL } from "node:os";
import process from "node:process";
import path from "node:path";
console.log("node", typeof EOL, typeof process.pid, path.join("a","b"));
