// THE ABI CONTRACT.
//
// Porffor only emits the builtins a compilation actually references, so this
// file decides what the backend can reach: a builtin not mentioned here (even
// transitively) is not in the linked object, and any v8__* shim that wants it
// gets `porf_embed_missing` at runtime rather than a link error.
//
// Keep it minimal-but-sufficient. Every entry here costs binary size in every
// consumer, which is the one thing porffor is unambiguously winning on.

const obj = { a: 1 };
obj.b = 'x';
Object.keys(obj);
Object.getOwnPropertyNames(obj);
Object.assign({}, obj);

const arr = [1, 2, 3];
arr.push(4);
arr.map(x => x).filter(x => x).join(',');

const s = ('he' + 'llo').slice(0, 4).toUpperCase();
String(1);
Number('1');

JSON.stringify(obj);
JSON.parse('{"a":1}');

const m = new Map(); m.set(1, 2); m.get(1);
const st = new Set(); st.add(1); st.has(1);
new Uint8Array(1);
new ArrayBuffer(8);
Promise.resolve(1);
Symbol('s');

// Nothing should actually print during a backend build; this exists only to
// keep the above reachable so porffor does not eliminate it.
if (globalThis.__porffor_never) console.log(obj, arr, s, m, st);
