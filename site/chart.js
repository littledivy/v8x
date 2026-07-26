// Minimal SVG progress chart, shared by the docs front page and /status/.
// v8xChart(el, base) fetches `${base}history.jsonl` and draws total passing
// tests over time. No dependencies; styling inherits the page's serif font.
async function v8xChart(el, base) {
  let history = [];
  try {
    const text = await fetch(base + "history.jsonl").then((r) => r.text());
    history = text
      .split("\n")
      .filter(Boolean)
      .map((l) => { try { return JSON.parse(l); } catch { return null; } })
      .filter(Boolean);
  } catch {}
  if (history.length < 2) {
    el.textContent = "progress chart appears after a few CI runs";
    return;
  }

  const W = 720, H = 170, PL = 40, PR = 14, PT = 10, PB = 22;
  const xs = history.map((s) => +new Date(s.ts));
  const ys = history.map((s) => s.totalPass);
  const x0 = Math.min(...xs), x1 = Math.max(...xs);
  const yMax = Math.max(...ys) * 1.08;
  const X = (t) => PL + ((t - x0) / (x1 - x0 || 1)) * (W - PL - PR);
  const Y = (v) => PT + (1 - v / yMax) * (H - PT - PB);

  let s = `<svg viewBox="0 0 ${W} ${H}" role="img" aria-label="tests passing over time">`;

  // y gridlines + labels at ~4 round values
  const step = niceStep(yMax / 4);
  for (let v = 0; v <= yMax; v += step) {
    const y = Y(v);
    s += `<line x1="${PL}" y1="${y}" x2="${W - PR}" y2="${y}" stroke="#ddd" stroke-width="1"/>`;
    s += `<text x="${PL - 6}" y="${y + 3.5}" text-anchor="end" font-size="11" fill="#666">${v}</text>`;
  }

  // x labels at ~5 dates
  for (let i = 0; i < 5; i++) {
    const t = x0 + ((x1 - x0) * i) / 4;
    const d = new Date(t);
    s += `<text x="${X(t)}" y="${H - 6}" text-anchor="middle" font-size="11" fill="#666">` +
      `${d.getDate()} ${d.toLocaleString("en", { month: "short" }).toLowerCase()}</text>`;
  }

  const pts = history.map((h) => `${X(+new Date(h.ts)).toFixed(1)},${Y(h.totalPass).toFixed(1)}`);
  s += `<polyline points="${pts.join(" ")}" fill="none" stroke="#000" stroke-width="1.5"/>`;

  // dot + value at the latest point
  const lx = X(xs[xs.length - 1]), ly = Y(ys[ys.length - 1]);
  s += `<circle cx="${lx}" cy="${ly}" r="3" fill="#000"/>`;
  s += `<text x="${lx - 6}" y="${ly - 7}" text-anchor="end" font-size="11" fill="#000">${ys[ys.length - 1]}</text>`;

  s += `</svg>`;
  el.innerHTML = s;

  function niceStep(raw) {
    const mag = Math.pow(10, Math.floor(Math.log10(raw)));
    for (const m of [1, 2, 5, 10]) if (raw <= m * mag) return m * mag;
    return 10 * mag;
  }
}
