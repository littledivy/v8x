import { Hono } from "npm:hono@4.5.0";
const app = new Hono();
app.get("/", (c) => c.text("hono-ok"));
console.log((await app.request("/")).status, await (await app.request("/")).text());
