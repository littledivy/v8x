import { nanoid } from "npm:nanoid@5.0.7";
console.log(typeof nanoid() === "string" && nanoid().length > 0);
