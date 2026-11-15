// Godspeed Code landing page. No dependencies; every feature degrades to the static HTML.
const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => [...r.querySelectorAll(s)];
const RM = matchMedia('(prefers-reduced-motion: reduce)').matches;
const REPO = 'ShriD5/godspeed-code';
const API = `https://api.github.com/repos/${REPO}`;
document.documentElement.classList.add('js');

// Frame indices in speedster-*.webp (order set by site/scripts/images.py).
const F = { jog: [0, 1, 2, 3], sprint: [4, 5, 6, 7], sleeping: [8, 9], idle: [10, 11], alert: [12, 13], celebrate: [14, 15] };
const setFrame = (el, i) => el.style.setProperty('--f', i);

function onView(el, fn, opts = {}) {
  if (!el) return;
  const io = new IntersectionObserver((entries) => {
    for (const e of entries) {
      fn(e.isIntersecting, e);
      if (e.isIntersecting && opts.once) io.disconnect();
    }
  }, { threshold: opts.threshold ?? 0.25, rootMargin: opts.rootMargin ?? '0px' });
  io.observe(el);
}

/* ---------- reveal on scroll (only for things below the fold) ---------- */
{
  const targets = $$('.agents-copy, .agents-list, .bench-row, .feat-copy, .feat-media, .tile, .quote, .star-note, .cadence, .needs-list > div, .dl-card, .closer-in');
  const fold = innerHeight;
  for (const el of targets) {
    if (RM || el.getBoundingClientRect().top < fold) continue;
    el.classList.add('rv');
    onView(el, (v) => v && el.classList.add('in'), { once: true, threshold: 0.12 });
  }
}

/* ---------- the runner: scroll progress with the speedster at its tip ---------- */
{
  const track = $('.track');
  const runner = $('.runner');
  if (track && runner) {
    let trackW = 0, runW = 0, lastY = scrollY, lastT = performance.now(), vel = 0, lastMove = 0;
    let mode = document.readyState === 'complete' ? 'idle' : 'jog', modeUntil = 0, frame = 0, nextFrame = 0;
    let celebrated = false, loading = mode === 'jog';
    const FPS = { jog: 9, sprint: 14, idle: 1.6, celebrate: 4 };
    const measure = () => { trackW = track.clientWidth; runW = runner.clientWidth; track.style.visibility = trackW < runW * 1.6 ? 'hidden' : ''; };
    const progress = () => { const max = document.documentElement.scrollHeight - innerHeight; return max > 0 ? Math.min(1, Math.max(0, scrollY / max)) : 0; };
    const place = () => {
      const p = progress();
      track.style.setProperty('--p', p.toFixed(4));
      runner.style.setProperty('--x', (p * Math.max(0, trackW - runW)).toFixed(1));
      return p;
    };
    measure(); place(); setFrame(runner, F.idle[0]);
    addEventListener('resize', () => { measure(); place(); }, { passive: true });
    if (!RM) {
      addEventListener('load', () => { loading = false; }, { once: true });
      addEventListener('scroll', () => {
        const now = performance.now(), y = scrollY, dy = y - lastY, dt = Math.max(1, now - lastT);
        vel = vel * 0.7 + (Math.abs(dy) / dt) * 0.3;
        if (dy) runner.style.setProperty('--dir', dy < 0 ? -1 : 1);
        lastY = y; lastT = now; lastMove = now;
        const p = place();
        if (p > 0.985 && !celebrated) { celebrated = true; mode = 'celebrate'; modeUntil = now + 1600; }
        if (p < 0.9) celebrated = false;
      }, { passive: true });
      const tick = (now) => {
        if (mode !== 'celebrate' || now > modeUntil) {
          if (now - lastMove < 380) mode = vel > 1.6 ? 'sprint' : 'jog';
          else mode = loading ? 'jog' : 'idle';
        }
        if (now >= nextFrame) {
          const frames = F[mode];
          frame = (frame + 1) % frames.length;
          setFrame(runner, frames[frame % frames.length]);
          nextFrame = now + 1000 / FPS[mode];
        }
        requestAnimationFrame(tick);
      };
      requestAnimationFrame(tick);
    } else {
      addEventListener('scroll', place, { passive: true });
    }
  }
}

/* ---------- hero video: the poster paints first, the loop loads after the page ---------- */
{
  const v = $('[data-hero-video]');
  const btn = $('[data-hero-toggle]');
  if (v && btn) {
    let userPaused = RM, attached = false, visible = true;
    const attach = () => {
      if (attached) return;
      attached = true;
      for (const s of $$('source', v)) s.src = s.dataset.src;
      v.preload = 'auto';
      v.load();
    };
    const play = () => { attach(); v.play().catch(() => {}); };
    const sync = () => {
      const paused = v.paused;
      btn.classList.toggle('paused', paused);
      btn.setAttribute('aria-label', paused ? 'Play video' : 'Pause video');
    };
    v.addEventListener('play', sync); v.addEventListener('pause', sync);
    v.addEventListener('playing', () => v.parentElement.classList.add('playing'), { once: true });
    btn.addEventListener('click', () => { if (v.paused) { userPaused = false; play(); } else { userPaused = true; v.pause(); } });
    const start = () => { if (!userPaused && visible) play(); };
    if (document.readyState === 'complete') setTimeout(start, 50); else addEventListener('load', () => setTimeout(start, 50), { once: true });
    onView(v, (vis) => { visible = vis; if (vis && attached && !userPaused) v.play().catch(() => {}); else if (!vis) v.pause(); }, { threshold: 0.1 });
    sync();
  }
}

/* ---------- feature clips: load when near, play once when visible ---------- */
for (const v of $$('[data-clip]')) {
  if (RM) continue;
  let loaded = false;
  onView(v, (vis) => {
    if (!vis) return;
    if (!loaded) { for (const s of $$('source', v)) s.src = s.dataset.src; v.load(); loaded = true; }
    try { v.currentTime = 0; } catch {}
    v.play().catch(() => {});
  }, { threshold: 0.55 });
}

/* ---------- Telegram mock and benchmark bars animate in ---------- */
onView($('[data-tg]'), (v) => v && $('[data-tg]').classList.add('in'), { once: true, threshold: 0.4 });
onView($('.bench'), (v) => v && $('.bench').classList.add('in'), { once: true, threshold: 0.2 });
if (RM) { $('[data-tg]')?.classList.add('in'); $('.bench')?.classList.add('in'); }

/* ---------- sprite animator (dock demo, footer nap) ---------- */
function animator(el) {
  let timer = 0, frames = [0], fps = 1, i = 0, on = false;
  const step = () => { setFrame(el, frames[i % frames.length]); i++; timer = setTimeout(step, 1000 / fps); };
  return {
    set(f, rate) { frames = f; fps = rate; i = 0; setFrame(el, f[0]); if (on && !RM) { clearTimeout(timer); step(); } },
    start() { if (on || RM) return; on = true; clearTimeout(timer); step(); },
    stop() { on = false; clearTimeout(timer); },
  };
}

{
  const demo = $('[data-dock]');
  if (demo) {
    const sprite = $('.sprite-lg', demo);
    const badge = $('.badge', demo);
    const state = $('.dock-state', demo);
    const buttons = $$('[data-mood]', demo);
    const MOODS = {
      sleeping: { f: F.sleeping, fps: 1.1, name: 'Asleep', desc: 'everything settled, napping' },
      jog: { f: F.jog, fps: 8, name: 'Jog', desc: '1 to 3 agents working' },
      sprint: { f: F.sprint, fps: 12, name: 'Sprint', desc: '4 or more agents working' },
      alert: { f: F.alert, fps: 2.4, name: 'Waving', desc: 'something needs you', badge: true },
      celebrate: { f: F.celebrate, fps: 3, name: 'Celebrating', desc: 'a chat just finished' },
    };
    const order = ['jog', 'sprint', 'alert', 'celebrate', 'sleeping'];
    const anim = animator(sprite);
    let current = 'jog', auto = !RM, cycle = 0;
    const show = (m) => {
      current = m;
      const mood = MOODS[m];
      anim.set(mood.f, mood.fps);
      badge.hidden = !mood.badge;
      state.innerHTML = '';
      const b = document.createElement('b'); b.textContent = mood.name;
      state.append(b, document.createTextNode(` ${mood.desc}`));
      for (const btn of buttons) btn.setAttribute('aria-pressed', String(btn.dataset.mood === m));
    };
    for (const btn of buttons) btn.addEventListener('click', () => { auto = false; clearInterval(cycle); show(btn.dataset.mood); });
    onView(demo, (v) => { if (v) demo.classList.add('loaded'); }, { once: true, rootMargin: '400px' });
    onView(demo, (v) => {
      if (v) {
        anim.start();
        if (auto && !cycle) cycle = setInterval(() => show(order[(order.indexOf(current) + 1) % order.length]), 3200);
      } else { anim.stop(); clearInterval(cycle); cycle = 0; }
    }, { threshold: 0.3 });
    show('jog');
  }

  const nap = $('.sprite-nap');
  if (nap) {
    const a = animator(nap);
    a.set(F.sleeping, 0.9);
    onView(nap, (v) => (v ? a.start() : a.stop()), { threshold: 0.5 });
  }
}

/* ---------- tour dialog ---------- */
{
  const dlg = $('[data-tour]');
  const video = $('[data-tour-video]');
  const hero = $('[data-hero-video]');
  let heroWasPlaying = false;
  if (dlg && video && typeof dlg.showModal === 'function') {
    for (const b of $$('[data-tour-open]')) b.addEventListener('click', () => {
      heroWasPlaying = !!hero && !hero.paused;
      if (!video.src) video.src = '/media/tour.mp4';
      dlg.showModal();
      hero?.pause();
      video.play().catch(() => {});
    });
    const close = () => dlg.close();
    $('[data-tour-close]').addEventListener('click', close);
    dlg.addEventListener('click', (e) => { if (e.target === dlg) close(); });
    dlg.addEventListener('close', () => {
      video.pause();
      if (heroWasPlaying) hero.play().catch(() => {});
    });
  } else {
    for (const b of $$('[data-tour-open]')) b.addEventListener('click', () => { location.href = '/media/tour.mp4'; });
  }
}

/* ---------- copy checksums ---------- */
for (const b of $$('[data-sha]')) {
  b.addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(b.dataset.full);
      const was = b.textContent;
      b.textContent = 'copied sha256';
      b.classList.add('copied');
      setTimeout(() => { b.textContent = was; b.classList.remove('copied'); }, 1400);
    } catch {}
  });
}

/* ---------- which OS is this? ---------- */
const os = (() => {
  const ua = navigator.userAgent;
  const platform = navigator.userAgentData?.platform || navigator.platform || '';
  // iPadOS Safari reports a Mac; it has touch and no userAgentData, unlike desktop Chrome.
  if (/iPhone|iPad|iPod/.test(ua) || (/Macintosh/.test(ua) && navigator.maxTouchPoints > 1 && !navigator.userAgentData)) return 'ios';
  if (/Android/.test(ua)) return 'android';
  if (/mac/i.test(platform)) return 'mac';
  if (/win/i.test(platform)) return 'windows';
  if (/linux|x11/i.test(platform)) return 'linux';
  return 'other';
})();
{
  const note = $('[data-dl-note]');
  const card = $(`[data-os-card="${os}"]`);
  if (card) { card.classList.add('mine'); $('[data-os-tag]', card).hidden = false; }
  const say = (html) => { if (note) note.innerHTML = html; };
  if (os === 'linux') {
    for (const a of $$('[data-dl-primary]')) a.href = '#download';
    for (const l of $$('[data-dl-label]')) l.textContent = 'Download for Linux';
    say('Linux builds are new and lightly tested. Mac is the main target. <a href="#download">All downloads</a>');
  } else if (os === 'windows') {
    say('Windows isn\'t supported yet. Godspeed runs on Apple silicon Macs, and Linux. <a href="#download">All downloads</a>');
  } else if (os === 'ios' || os === 'android') {
    say('On your phone? Grab it on your Mac. Free and MIT-licensed. <a href="#download">All downloads</a>');
  }
}

/* ---------- live GitHub data, with the static HTML as the fallback ---------- */
const FALLBACK_RELEASES = [
  ['v0.5.9', '2026-10-28T19:01:12Z'], ['v0.5.8', '2026-10-28T10:57:08Z'], ['v0.5.7', '2026-10-27T13:43:03Z'],
  ['v0.5.6', '2026-10-26T14:10:54Z'], ['v0.5.5', '2026-10-20T14:18:21Z'], ['v0.5.4', '2026-10-20T11:10:03Z'],
  ['v0.5.3', '2026-10-18T22:30:04Z'], ['v0.5.2', '2026-10-18T14:46:43Z'], ['v0.5.1', '2026-10-18T12:39:48Z'],
  ['v0.5.0', '2026-10-18T08:27:19Z'], ['v0.4.0', '2026-10-17T14:27:40Z'], ['v0.3.0', '2026-10-16T13:26:21Z'],
  ['v0.2.0', '2026-10-16T12:22:12Z'], ['v0.1.0', '2026-10-16T11:05:05Z'],
].map(([tag, at]) => ({ tag, at: new Date(at) }));

async function getJSON(url, key, ttlMin = 10) {
  try {
    const hit = JSON.parse(localStorage.getItem(key) || 'null');
    if (hit && Date.now() - hit.t < ttlMin * 60e3) return hit.d;
  } catch {}
  const r = await fetch(url, { headers: { Accept: 'application/vnd.github+json' } });
  if (!r.ok) throw new Error(String(r.status));
  const d = await r.json();
  try { localStorage.setItem(key, JSON.stringify({ t: Date.now(), d })); } catch {}
  return d;
}

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
const dayKey = (d) => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
const daysBetween = (a, b) => Math.round((dayKey(b) - dayKey(a)) / 864e5);
const short = (d) => `${d.getDate()} ${MONTHS[d.getMonth()]}`;
const ago = (d) => {
  const n = daysBetween(d, new Date());
  return n <= 0 ? 'today' : n === 1 ? 'yesterday' : n < 7 ? `${n} days ago` : short(d);
};
const setAll = (sel, text) => { for (const el of $$(sel)) el.textContent = text; };
const fmtCount = (n) => (n >= 1000 ? `${(n / 1000).toFixed(n >= 10000 ? 0 : 1)}k` : String(n));

function renderReleases(list) {
  if (!list.length) return;
  const latest = list[0];
  const first = list[list.length - 1];
  setAll('[data-version]', latest.tag);
  setAll('[data-rel-latest]', latest.tag);
  setAll('[data-released]', `shipped ${ago(latest.at)}`);
  setAll('[data-released-date]', `${short(latest.at)} ${latest.at.getFullYear()}`);
  setAll('[data-rel-count]', String(list.length));
  setAll('[data-rel-span]', `in ${daysBetween(first.at, latest.at) + 1} days`);
  setAll('[data-rel-ago]', ago(latest.at));
  setAll('[data-axis-start]', short(first.at));
  drawChart(list);
}

function drawChart(list) {
  const svg = $('.chart-svg');
  if (!svg) return;
  const NS = 'http://www.w3.org/2000/svg';
  const w = Math.max(240, svg.clientWidth || 600), h = 120, base = 110, pad = 8;
  svg.setAttribute('viewBox', `0 0 ${w} ${h}`);
  svg.removeAttribute('preserveAspectRatio');
  svg.replaceChildren();
  const start = list[list.length - 1].at, span = Math.max(1, daysBetween(start, new Date()));
  const el = (tag, attrs, parent = svg) => { const n = document.createElementNS(NS, tag); for (const k in attrs) n.setAttribute(k, attrs[k]); parent.append(n); return n; };
  el('line', { x1: 0, x2: w, y1: base, y2: base, class: 'base' });
  for (let d = 0; d <= span; d++) el('line', { x1: pad + (d / span) * (w - 2 * pad), x2: pad + (d / span) * (w - 2 * pad), y1: base - 3, y2: base + 3, class: 'base' });
  const perDay = new Map();
  const ordered = [...list].reverse();
  ordered.forEach((r, i) => {
    const d = daysBetween(start, r.at);
    const k = perDay.get(d) || 0; perDay.set(d, k + 1);
    const x = pad + (d / span) * (w - 2 * pad), y = base - 14 - k * 19;
    if (k === 0) el('line', { x1: x, x2: x, y1: base, y2: y, class: 'stem' });
    else el('line', { x1: x, x2: x, y1: y + 19, y2: y, class: 'stem' });
    const isLatest = i === ordered.length - 1;
    const c = el('circle', { cx: x, cy: y, r: isLatest ? 6.5 : 5, fill: isLatest ? '#ffd88a' : '#f2b63f', opacity: isLatest ? 1 : 0.85 });
    if (isLatest) c.setAttribute('style', 'filter: drop-shadow(0 0 6px rgba(242,182,63,.9))');
    el('title', {}, c).textContent = `${r.tag} · ${short(r.at)}`;
  });
}

function renderAssets(rel) {
  const pick = (re) => (rel.assets || []).find((a) => re.test(a.name));
  const map = { dmg: /^Godspeed-macos-arm64\.dmg$/, app: /-macos-arm64-app\.tar\.gz$/, x64: /-linux-x86_64\.tar\.gz$/, arm: /-linux-aarch64\.tar\.gz$/ };
  for (const [key, re] of Object.entries(map)) {
    const a = pick(re);
    if (!a) continue;
    for (const link of $$(`[data-asset="${key}"]`)) link.href = a.browser_download_url;
    const mb = `${Math.round(a.size / 1048576)} MB`;
    for (const s of $$(`[data-size="${key}"]`)) s.textContent = key === 'x64' || key === 'arm' ? `${mb} · tar.gz` : mb;
    const sha = a.digest?.startsWith('sha256:') ? a.digest.slice(7) : '';
    for (const b of $$(`[data-sha="${key}"]`)) {
      if (sha) { b.dataset.full = sha; b.textContent = `sha256 ${sha.slice(0, 6)}…${sha.slice(-4)}`; }
      else b.hidden = true;
    }
  }
  if (os === 'linux') { const x = pick(map.x64); if (x) for (const a of $$('[data-dl-primary]')) a.href = x.browser_download_url; }
  for (const a of $$('[data-release-link]')) a.href = rel.html_url;
}

renderReleases(FALLBACK_RELEASES);
let chartData = FALLBACK_RELEASES;
addEventListener('resize', () => drawChart(chartData), { passive: true });

getJSON(API, 'gs:repo').then((r) => { if (Number.isFinite(r.stargazers_count)) setAll('[data-stars]', fmtCount(r.stargazers_count)); }).catch(() => {});
getJSON(`${API}/releases?per_page=100`, 'gs:releases').then((rels) => {
  const list = rels.filter((r) => !r.draft && !r.prerelease && r.published_at).map((r) => ({ tag: r.tag_name, at: new Date(r.published_at), raw: r }));
  if (!list.length) return;
  list.sort((a, b) => b.at - a.at);
  chartData = list;
  renderReleases(list);
  renderAssets(list[0].raw);
}).catch(() => {});
getJSON(`https://api.github.com/search/issues?q=${encodeURIComponent(`repo:${REPO} is:pr is:merged`)}&per_page=1`, 'gs:prs', 30)
  .then((r) => { if (Number.isFinite(r.total_count)) setAll('[data-prs]', String(r.total_count)); }).catch(() => {});

/* ---------- iPhone beta waitlist ---------- */
// The form posts to the edge's /waitlist without JS (it redirects back with
// ?beta=…); with JS it stays on the page.
{
  const form = $('[data-beta-form]');
  const status = $('[data-beta-status]');
  const say = (text, err = false) => {
    if (!status) return;
    status.textContent = text;
    status.classList.toggle('err', err);
  };
  const JOINED = "You're on the list. We'll email your TestFlight invite when there's room.";
  const back = new URLSearchParams(location.search).get('beta');
  if (back === 'joined') say(JOINED);
  else if (back === 'invalid') say('That email didn\'t look right. Try again?', true);
  else if (back === 'busy') say('Too many tries right now. Try again in a minute.', true);
  form?.addEventListener('submit', async (e) => {
    e.preventDefault();
    const data = new FormData(form);
    const body = { email: String(data.get('email') || '').trim() };
    const note = String(data.get('note') || '').trim();
    if (note) body.note = note;
    const hp = String(data.get('website') || '');
    if (hp) body.website = hp;
    const button = form.querySelector('button');
    button.disabled = true;
    say('Sending…');
    try {
      const res = await fetch(form.action, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(body),
      });
      const out = await res.json().catch(() => ({}));
      if (res.ok) {
        form.reset();
        say(JOINED);
      } else {
        say(out.message || 'Something went wrong. Try again later.', true);
      }
    } catch {
      say("Couldn't reach the server. Check your connection and try again.", true);
    } finally {
      button.disabled = false;
    }
  });
}
