/**
 * The admin dashboard page: server-rendered HTML with inline SVG charts and
 * no script (the CSP forbids it). Hover details ride SVG `<title>` tooltips;
 * the "Daily numbers" table is the accessible view of every chart.
 * Godspeed's look: charcoal cards, warm gold accent, big numbers.
 */
import type { DayValue, RetentionCohort, Share, Stats, UsageDay } from "./admin-stats";
import { RANGES } from "./admin-stats";

const GOLD = "#e8b75c";
/** Categorical slots for the retention lines (validated on the card surface). */
const SERIES = { d1: "#3987e5", d7: "#d95926", d30: "#199e70" } as const;

const esc = (s: string): string =>
  s
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");

export const compact = (n: number): string => {
  const abs = Math.abs(n);
  const fmt = (v: number, suffix: string) => `${v >= 100 ? v.toFixed(0) : v >= 10 ? v.toFixed(1) : v.toFixed(2)}${suffix}`;
  if (abs >= 1e12) return fmt(n / 1e12, "T");
  if (abs >= 1e9) return fmt(n / 1e9, "B");
  if (abs >= 1e6) return fmt(n / 1e6, "M");
  if (abs >= 1e4) return fmt(n / 1e3, "K");
  if (Number.isInteger(n) || abs >= 100) return Math.round(n).toLocaleString("en-US");
  return n.toFixed(1).replace(/\.0$/, "");
};

export const usd = (n: number): string =>
  n >= 1000
    ? `$${Math.round(n).toLocaleString("en-US")}`
    : n >= 10
      ? `$${n.toFixed(0)}`
      : `$${n.toFixed(2)}`;

const pct = (n: number): string => `${Math.round(n * 100)}%`;

/** "+12% w/w" (or "new" / "flat"). */
export const weekOverWeek = (current: number, previous: number): string => {
  if (previous === 0) return current === 0 ? "flat w/w" : "new this week";
  const change = (current - previous) / previous;
  const arrow = change > 0.005 ? "▲" : change < -0.005 ? "▼" : "·";
  const sign = change > 0 ? "+" : "";
  return `${arrow} ${sign}${Math.round(change * 100)}% w/w`;
};

const shortDay = (day: string): string => {
  const [, m, d] = day.split("-");
  const months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
  return `${months[Number(m) - 1]} ${Number(d)}`;
};

// ---------------------------------------------------------------------------
// Charts
// ---------------------------------------------------------------------------

const W = 320;
const H = 72;

/** Daily bars anchored to the baseline, 2px gaps, rounded data ends. */
const bars = (values: DayValue[], format: (n: number) => string, label: string): string => {
  const max = Math.max(1, ...values.map((v) => v.value));
  const slot = W / Math.max(1, values.length);
  const gap = values.length > 45 ? 1 : 2;
  const width = Math.max(1, slot - gap);
  const radius = Math.min(3, width / 2);
  const rects = values
    .map((v, i) => {
      const h = v.value > 0 ? Math.max(2, (v.value / max) * (H - 4)) : 0;
      const x = i * slot;
      const y = H - h;
      const hit = `<rect x="${x.toFixed(1)}" y="0" width="${slot.toFixed(1)}" height="${H}" fill="transparent"><title>${shortDay(v.day)}: ${esc(format(v.value))}</title></rect>`;
      if (h === 0) return `<rect x="${x.toFixed(1)}" y="${H - 1}" width="${width.toFixed(1)}" height="1" fill="#3a3229"/>${hit}`;
      // Round only the top (data end); the base sits flat on the axis.
      const r = Math.min(radius, h);
      const path = `M${x.toFixed(1)},${H} V${(y + r).toFixed(1)} Q${x.toFixed(1)},${y.toFixed(1)} ${(x + r).toFixed(1)},${y.toFixed(1)} H${(x + width - r).toFixed(1)} Q${(x + width).toFixed(1)},${y.toFixed(1)} ${(x + width).toFixed(1)},${(y + r).toFixed(1)} V${H} Z`;
      return `<path d="${path}" fill="${GOLD}" opacity="0.9"/>${hit}`;
    })
    .join("");
  return `<svg class="chart" viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" role="img" aria-label="${esc(label)}">${rects}</svg>`;
};

/** A 2px line over a soft area, with per-day hover targets. */
const area = (values: DayValue[], format: (n: number) => string, label: string, id: string): string => {
  if (values.length === 0) return "";
  const max = Math.max(1, ...values.map((v) => v.value));
  const step = values.length > 1 ? W / (values.length - 1) : 0;
  const pts = values.map((v, i) => [i * step, H - 3 - (v.value / max) * (H - 8)] as const);
  const line = pts.map(([x, y], i) => `${i ? "L" : "M"}${x.toFixed(1)},${y.toFixed(1)}`).join(" ");
  const fill = `${line} L${W},${H} L0,${H} Z`;
  const last = pts[pts.length - 1];
  const hits = values
    .map((v, i) => {
      const x = Math.max(0, i * step - step / 2);
      return `<rect x="${x.toFixed(1)}" y="0" width="${Math.max(step, 4).toFixed(1)}" height="${H}" fill="transparent"><title>${shortDay(v.day)}: ${esc(format(v.value))}</title></rect>`;
    })
    .join("");
  return `<svg class="chart" viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" role="img" aria-label="${esc(label)}">
<defs><linearGradient id="g-${id}" x1="0" x2="0" y1="0" y2="1"><stop offset="0" stop-color="${GOLD}" stop-opacity="0.28"/><stop offset="1" stop-color="${GOLD}" stop-opacity="0"/></linearGradient></defs>
<path d="${fill}" fill="url(#g-${id})"/><path d="${line}" fill="none" stroke="${GOLD}" stroke-width="2" vector-effect="non-scaling-stroke" stroke-linejoin="round"/>
<circle cx="${last[0].toFixed(1)}" cy="${last[1].toFixed(1)}" r="3" fill="${GOLD}" stroke="#17130f" stroke-width="2" vector-effect="non-scaling-stroke"/>${hits}</svg>`;
};

/** Horizontal share bars, one hue, labelled with value and share. */
const shares = (rows: Share[], empty: string, format: (n: number) => string = compact, limit = 8): string => {
  if (rows.length === 0) return `<p class="empty">${esc(empty)}</p>`;
  const shown = rows.slice(0, limit);
  const rest = rows.slice(limit).reduce((n, r) => n + r.value, 0);
  if (rest > 0) shown.push({ key: "Other", value: rest });
  const total = rows.reduce((n, r) => n + r.value, 0) || 1;
  const max = Math.max(...shown.map((r) => r.value), 1);
  return `<div class="shares">${shown
    .map(
      (r) => `<div class="share" title="${esc(r.key)}: ${esc(format(r.value))} (${pct(r.value / total)})">
<span class="share-key">${esc(r.key)}</span>
<span class="share-bar"><span style="width:${((r.value / max) * 100).toFixed(1)}%"></span></span>
<span class="share-val">${esc(format(r.value))} <em>${pct(r.value / total)}</em></span></div>`
    )
    .join("")}</div>`;
};

/** D1 / D7 / D30 retention by first-seen cohort: three lines, one axis. */
const retentionChart = (cohorts: RetentionCohort[]): string => {
  const rows = cohorts.filter((c) => c.size > 0);
  if (rows.length === 0) return `<p class="empty">No cohorts yet: retention needs installs at least a day old.</p>`;
  const w = 680;
  const h = 180;
  const padL = 34;
  const padR = 44;
  const padT = 10;
  const padB = 22;
  const step = rows.length > 1 ? (w - padL - padR) / (rows.length - 1) : 0;
  const x = (i: number) => padL + i * step;
  const y = (rate: number) => padT + (1 - rate) * (h - padT - padB);
  const grid = [0, 0.25, 0.5, 0.75, 1]
    .map(
      (g) =>
        `<line x1="${padL}" x2="${w - padR}" y1="${y(g).toFixed(1)}" y2="${y(g).toFixed(1)}" stroke="#2a241d" stroke-width="1"/><text x="${padL - 6}" y="${(y(g) + 4).toFixed(1)}" text-anchor="end" class="axis">${pct(g)}</text>`
    )
    .join("");
  const series = (["d1", "d7", "d30"] as const)
    .map((key) => {
      const pts = rows
        .map((c, i) => (c[key] === null ? null : ([x(i), y(c[key] as number), c] as const)))
        .filter((p): p is readonly [number, number, RetentionCohort] => p !== null);
      if (pts.length === 0) return "";
      const d = pts.map(([px, py], i) => `${i ? "L" : "M"}${px.toFixed(1)},${py.toFixed(1)}`).join(" ");
      const dots = pts
        .map(
          ([px, py, c]) =>
            `<circle cx="${px.toFixed(1)}" cy="${py.toFixed(1)}" r="4" fill="${SERIES[key]}" stroke="#17130f" stroke-width="2"><title>${shortDay(c.cohort)} cohort (${c.size}): ${key.toUpperCase()} ${pct(c[key] as number)}</title></circle>`
        )
        .join("");
      const [lx, ly] = pts[pts.length - 1];
      return `<path d="${d}" fill="none" stroke="${SERIES[key]}" stroke-width="2" stroke-linejoin="round"/>${dots}<text x="${(lx + 8).toFixed(1)}" y="${(ly + 4).toFixed(1)}" class="direct">${key.toUpperCase()}</text>`;
    })
    .join("");
  const first = rows[0].cohort;
  const last = rows[rows.length - 1].cohort;
  const legend = (["d1", "d7", "d30"] as const)
    .map((k) => `<span class="legend-item"><i style="background:${SERIES[k]}"></i>${k.toUpperCase()}</span>`)
    .join("");
  return `<div class="legend">${legend}</div><svg class="wide" viewBox="0 0 ${w} ${h}" role="img" aria-label="Retention by first-seen cohort">${grid}${series}
<text x="${padL}" y="${h - 4}" class="axis">${shortDay(first)}</text><text x="${w - padR}" y="${h - 4}" text-anchor="end" class="axis">${shortDay(last)}</text></svg>`;
};

// ---------------------------------------------------------------------------
// Tiles and page
// ---------------------------------------------------------------------------

const tile = (label: string, value: string, sub: string, chart: string, wide = false): string =>
  `<section class="tile${wide ? " wide" : ""}"><h2>${esc(label)}</h2><div class="value">${esc(value)}</div><div class="sub" title="${esc(sub)}">${esc(sub)}</div>${chart}</section>`;

const perDay = (rows: UsageDay[], pick: (u: UsageDay) => number): DayValue[] =>
  rows.map((u) => ({ day: u.day, value: pick(u) }));

export const renderDashboard = (stats: Stats): string => {
  const { installs, dau, wau, usage, github } = stats;
  const avg = (n: number) => n / 7;
  const picker = RANGES.map(
    (r) => `<a href="/admin?range=${r}" class="${r === stats.range ? "on" : ""}">${r}d</a>`
  ).join("");
  const wauShare = wau.monthly > 0 ? `${pct(wau.current / wau.monthly)} of ${compact(wau.monthly)} monthly` : "no monthly actives yet";
  const releases = github.releases
    .map((r) => `<div class="share"><span class="share-key">${esc(r.tag)}</span><span class="share-val">${compact(r.downloads)}</span></div>`)
    .join("");

  const hero = [
    tile(
      "Total installs",
      compact(installs.total),
      `+${compact(installs.newYesterday)} new yesterday`,
      bars(installs.newPerDay, (n) => `${compact(n)} new`, "New installs per day")
    ),
    tile(
      "Daily active installs",
      compact(dau.yesterday),
      `${weekOverWeek(dau.yesterday, dau.weekBefore)} · yesterday${dau.backgroundYesterday ? ` · +${compact(dau.backgroundYesterday)} background` : ""}`,
      area(dau.perDay, (n) => `${compact(n)} active`, "Daily active installs", "dau")
    ),
    tile(
      "Weekly active installs",
      compact(wau.current),
      wauShare,
      area(wau.perDay, (n) => `${compact(n)} weekly active`, "Weekly active installs, rolling 7 days", "wau")
    ),
    tile(
      "Spend / day",
      usd(avg(usage.last7.spend)),
      `at API prices · 7-day avg · ${weekOverWeek(usage.last7.spend, usage.prev7.spend)}`,
      bars(perDay(usage.perDay, (u) => u.spend), usd, "Estimated spend per day at API prices")
    )
  ].join("");

  const extras = [
    tile(
      "Tokens / day",
      compact(avg(usage.last7.tokens)),
      `7-day avg · ${weekOverWeek(usage.last7.tokens, usage.prev7.tokens)} · ${pct(usage.last7.tokens ? usage.last7.cacheReadTokens / usage.last7.tokens : 0)} cache reads`,
      area(perDay(usage.perDay, (u) => u.tokens), (n) => `${compact(n)} tokens`, "Tokens per day", "tok")
    ),
    tile(
      "Turns / day",
      compact(avg(usage.last7.turns)),
      `7-day avg · ${weekOverWeek(usage.last7.turns, usage.prev7.turns)} · ${compact(avg(usage.last7.chats))} chats/day`,
      bars(perDay(usage.perDay, (u) => u.turns), (n) => `${compact(n)} turns`, "Turns finished per day")
    ),
    tile(
      "Zoomie runs / day",
      compact(avg(usage.last7.zoomieRuns)),
      `7-day avg · ${weekOverWeek(usage.last7.zoomieRuns, usage.prev7.zoomieRuns)} · ${compact(avg(usage.last7.approvals))} approvals/day`,
      bars(perDay(usage.perDay, (u) => u.zoomieRuns), (n) => `${compact(n)} runs`, "Zoomie runs per day")
    ),
    tile(
      "Update checks",
      compact(stats.updateChecks.total),
      `legacy feed hits, ${stats.range}d${github.manifestDownloads !== null ? ` · ${compact(github.manifestDownloads)} GitHub manifest pulls all-time` : ""}`,
      bars(stats.updateChecks.perDay, (n) => `${compact(n)} checks`, "Legacy update-feed hits per day")
    )
  ].join("");

  const tableRows = usage.perDay
    .map((u, i) => {
      const d = dau.perDay[i]?.value ?? 0;
      const n = installs.newPerDay[i]?.value ?? 0;
      return `<tr><td>${u.day}</td><td>${n}</td><td>${d}</td><td>${wau.perDay[i]?.value ?? 0}</td><td>${usd(u.spend)}</td><td>${compact(u.tokens)}</td><td>${u.turns}</td><td>${u.zoomieRuns}</td><td>${stats.updateChecks.perDay[i]?.value ?? 0}</td></tr>`;
    })
    .reverse()
    .join("");

  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="robots" content="noindex">
<title>Godspeed Code · admin</title>
<style>
:root{--bg:#0c0a08;--card:#17130f;--line:#2a241d;--ink:#ece6dc;--ink2:#b4ab9d;--ink3:#857c70;--gold:${GOLD};--gold-ink:#f3d9a4}
*{box-sizing:border-box}
body{margin:0;background:radial-gradient(1200px 600px at 10% -10%,#221a10 0,transparent 60%),var(--bg);color:var(--ink);font:14px/1.5 ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif;min-height:100vh}
main{max-width:1240px;margin:0 auto;padding:28px 24px 48px}
header{display:flex;align-items:center;gap:16px;flex-wrap:wrap;margin-bottom:22px}
header h1{font-size:18px;font-weight:600;margin:0;color:var(--gold-ink);letter-spacing:.01em}
header .meta{color:var(--ink3);font-size:12px}
.picker{margin-left:auto;display:flex;gap:2px;background:var(--card);border:1px solid var(--line);border-radius:9px;padding:3px}
.picker a{color:var(--ink2);text-decoration:none;padding:4px 10px;border-radius:6px;font-size:12px;font-variant-numeric:tabular-nums}
.picker a.on{background:#2b2318;color:var(--gold-ink)}
header .out{color:var(--ink3);font-size:12px}
.grid{display:grid;grid-template-columns:repeat(4,minmax(0,1fr));gap:14px;margin-bottom:14px}
@media (max-width:1000px){.grid{grid-template-columns:repeat(2,minmax(0,1fr))}}
@media (max-width:560px){.grid{grid-template-columns:1fr}}
.tile,.panel{background:var(--card);border:1px solid var(--line);border-radius:14px;padding:16px 16px 12px;min-width:0}
.tile h2,.panel h2{margin:0;font-size:11px;font-weight:600;letter-spacing:.09em;text-transform:uppercase;color:var(--ink3)}
.value{font-size:34px;font-weight:600;letter-spacing:-.02em;margin:6px 0 0;font-variant-numeric:tabular-nums}
.sub{color:var(--ink2);font-size:12px;margin:2px 0 12px;font-variant-numeric:tabular-nums;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.chart{display:block;width:100%;height:72px}
.panels{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:14px;margin-bottom:14px}
@media (max-width:1000px){.panels{grid-template-columns:1fr}}
.panel h2{margin-bottom:12px}
.shares{display:flex;flex-direction:column;gap:7px}
.share{display:grid;grid-template-columns:minmax(70px,1fr) 2fr auto;gap:10px;align-items:center;font-size:12px}
.share-key{color:var(--ink);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.share-bar{height:8px;background:#221c15;border-radius:4px;overflow:hidden}
.share-bar span{display:block;height:100%;background:var(--gold);border-radius:0 4px 4px 0}
.share-val{color:var(--ink2);font-variant-numeric:tabular-nums;text-align:right}
.share-val em{color:var(--ink3);font-style:normal;margin-left:4px}
.empty{color:var(--ink3);font-size:12px;margin:4px 0}
.row2{display:grid;grid-template-columns:2fr 1fr;gap:14px;margin-bottom:14px}
@media (max-width:1000px){.row2{grid-template-columns:1fr}}
.wide{width:100%;height:auto;display:block}
.axis{fill:var(--ink3);font-size:10px}
.direct{fill:var(--ink2);font-size:11px;font-weight:600}
.legend{display:flex;gap:14px;margin:-4px 0 6px;font-size:12px;color:var(--ink2)}
.legend i{display:inline-block;width:10px;height:10px;border-radius:3px;margin-right:6px;vertical-align:-1px}
.big{font-size:28px;font-weight:600;font-variant-numeric:tabular-nums}
.kv{display:flex;gap:28px;margin-bottom:12px}
.kv div span{display:block;color:var(--ink3);font-size:11px;text-transform:uppercase;letter-spacing:.08em}
details{background:var(--card);border:1px solid var(--line);border-radius:14px;padding:12px 16px}
summary{cursor:pointer;color:var(--ink2);font-size:12px;letter-spacing:.06em;text-transform:uppercase}
table{width:100%;border-collapse:collapse;margin-top:10px;font-size:12px;font-variant-numeric:tabular-nums}
th,td{text-align:right;padding:5px 8px;border-bottom:1px solid var(--line)}
th:first-child,td:first-child{text-align:left}
th{color:var(--ink3);font-weight:600}
td{color:var(--ink2)}
.note{color:var(--ink3);font-size:11px;margin-top:16px}
</style></head>
<body><main>
<header><h1>Godspeed Code</h1><span class="meta">Anonymous usage · UTC · updated ${esc(stats.generatedAt.slice(0, 16).replace("T", " "))}</span>
<nav class="picker" aria-label="Range">${picker}</nav><a class="out" href="/admin/access">Beta access</a><a class="out" href="/admin/logout">Sign out</a></header>
<div class="grid">${hero}</div>
<div class="grid">${extras}</div>
<div class="panels">
<section class="panel"><h2>Version adoption · last 7 days</h2>${shares(stats.versions, "No pings in the last 7 days.")}</section>
<section class="panel"><h2>OS / arch · last 7 days</h2>${shares(stats.platforms, "No pings in the last 7 days.")}</section>
<section class="panel"><h2>Harness mix · turns, ${stats.range}d</h2>${shares(stats.harnesses, "No turns reported yet.")}</section>
</div>
<div class="row2">
<section class="panel"><h2>Retention by first-seen cohort</h2>${retentionChart(stats.retention)}</section>
<section class="panel"><h2>GitHub</h2>
<div class="kv"><div><span>Stars</span><div class="big">${github.stars === null ? "–" : compact(github.stars)}</div></div><div><span>Release downloads</span><div class="big">${github.downloads === null ? "–" : compact(github.downloads)}</div></div></div>
<div class="shares">${releases || `<p class="empty">No release data.</p>`}</div>
${github.error ? `<p class="empty">GitHub: ${esc(github.error)}${github.fetchedAt ? " (showing cached)" : ""}</p>` : ""}
</section>
</div>
<details><summary>Daily numbers</summary><table><thead><tr><th>Day</th><th>New</th><th>DAU</th><th>WAU</th><th>Spend</th><th>Tokens</th><th>Turns</th><th>Zoomies</th><th>Update checks</th></tr></thead><tbody>${tableRows}</tbody></table></details>
<p class="note">Spend is an estimate at list API prices; subscription logins pay nothing per token. Usage for a day arrives with each install's next-day ping, so the latest days fill in over time. Daily actives count installs a person used; background engines are counted apart.</p>
</main></body></html>`;
};
