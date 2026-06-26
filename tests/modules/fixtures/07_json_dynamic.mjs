const d = await import("./_helpers/data.json", { with: { type: "json" } });
console.log("dynjson", d.default.v);
