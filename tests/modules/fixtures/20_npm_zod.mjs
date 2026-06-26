import { z } from "npm:zod@3.23.8";
console.log(z.object({ n: z.string() }).parse({ n: "ok" }).n);
