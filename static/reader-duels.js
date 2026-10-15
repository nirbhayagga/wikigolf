/* The article reader — free to open, costs clock time; article links are
   inert so it can never become a back door past the scoring. The HTML comes
   from Wikipedia's REST API live, which is presentation only: the game's
   links, par and routes all stay on the shipped snapshot. */
function closeReader() { closeOverlay('reader'); $('rbody').innerHTML = ''; }
async function openReader() {
  const a = state.cur; if (!a) return;
  const slug = encodeURIComponent(a.title.replace(/ /g, '_'));
  openOverlay('reader');
  $('rtitle').textContent = a.title;
  $('rlink').href = `https://en.wikipedia.org/wiki/${slug}`;
  $('rbody').innerHTML = '<div class="empty">loading&hellip;</div>';
  try {
    const res = await fetch(`https://en.wikipedia.org/api/rest_v1/page/html/${slug}`);
    if (!res.ok) throw new Error(`Wikipedia answered ${res.status}`);
    const doc = new DOMParser().parseFromString(await res.text(), 'text/html');
    const allowed = new Set('p div span section article h1 h2 h3 h4 h5 h6 ul ol li dl dt dd table tbody thead tr td th caption b strong i em small sup sub br hr blockquote img a figure figcaption'.split(' '));
    for (const el of [...doc.body.querySelectorAll('*')]) {
      if (!allowed.has(el.tagName.toLowerCase())) { el.remove(); continue; }
      for (const attr of [...el.attributes]) {
        if (!['href','src','alt','title','colspan','rowspan'].includes(attr.name)) el.removeAttribute(attr.name);
      }
      if (el.hasAttribute('src')) {
        try { const url=new URL(el.getAttribute('src'), 'https://en.wikipedia.org');
          if(url.protocol !== 'https:' || url.hostname !== 'upload.wikimedia.org') el.removeAttribute('src');
          else el.setAttribute('src',url.href);
        } catch { el.removeAttribute('src'); }
      }
    }
    if (state.cur !== a || state.veil || $('reader').hidden) return;
    $('rbody').innerHTML = doc.body.innerHTML;
    $('rbody').scrollTop = 0;
    // Links that exist in this race's graph become playable — a click here
    // goes through go(), so it counts exactly like a click in the list.
    // Links via redirect aliases don't match a graph title and stay inert.
    const byTitle = new Map((a.links || []).map(l => [l.title, l]));
    for (const el of $('rbody').querySelectorAll('a')) {
      const href = el.getAttribute('href') || '';
      el.removeAttribute('href');
      if (!href.startsWith('./')) continue;
      let t;
      try { t = decodeURIComponent(href.slice(2).split('#')[0]).replace(/_/g, ' '); }
      catch { continue; }
      const link = byTitle.get(t);
      if (!link || link.banned) continue;
      el.classList.add('ingraph');
      el.setAttribute('href','#');
      el.addEventListener('click', ev => {
        ev.preventDefault(); closeReader(); go(link.id).catch(()=>{});
      });
    }
  } catch (e) {
    $('rbody').innerHTML =
      `<div class="empty">couldn't load the article &mdash; ${esc(e.message)}</div>`;
  }
}
$('readbtn').onclick = openReader;
$('rclose').onclick = closeReader;
$('reader').onclick = e => { if (e.target.id === 'reader') closeReader(); };

/* ---------------- duels (live server only, --enable-duels) ----------------
   The server relays frames and referees presence; the room's race is
   adopted as a normal issued run — same from/to for everyone, par
   recomputed identically — so the clock, compass and card behave exactly
   like any race. Duels never touch the leaderboard. */
let DUEL = null;
function duelStop() {
  if (!DUEL) return;
  const d = DUEL; DUEL = null;
  try { d.ws.close(); } catch {}
  $('duelbar').innerHTML = '';
}
function duelRender() {
  if (!DUEL) return;
  const rows = Object.entries(DUEL.progress).map(([n, p]) =>
    `<span class="chip"><b>${esc(n)}</b> ${p.done ? '&#127937;' : ''} ${p.clicks ?? 0} clicks</span>`);
  $('duelbar').innerHTML =
    `<span class="chip copy" title="Copy the room code">room <b>${esc(DUEL.code)}</b> &#10697;</span>`
    + rows.join('');
}
async function duelCopy(code, flash) {
  if (!code) return;
  try { await navigator.clipboard.writeText(code); }
  catch {
    // Clipboard API needs a secure context; the LAN-http fallback still works.
    $('duelcode').value = code; $('duelcode').select();
    try { document.execCommand('copy'); } catch {}
  }
  if (flash) { const keep = flash.innerHTML; flash.innerHTML = '&#10003;'; setTimeout(() => { flash.innerHTML = keep; }, 1200); }
}
function duelConnect(code, name) {
  duelStop();
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  const ws = new WebSocket(
    `${proto}://${location.host}/api/duel/ws/${encodeURIComponent(code)}?name=${encodeURIComponent(name)}`);
  DUEL = { code, name, ws, progress: {}, adopting: false };
  ws.onmessage = async ev => {
    if (!DUEL || DUEL.ws !== ws) return;
    let m; try { m = JSON.parse(ev.data); } catch { return; }
    if (m.type === 'error') { $('duelerr').textContent = m.error; duelStop(); return; }
    if (m.type === 'room') {
      $('duelerr').textContent = '';
      DUEL.adopting = true;
      try { await startRace('/api/puzzle', `from=${m.start.id}&to=${m.goal.id}`); }
      catch {} finally { if (DUEL) DUEL.adopting = false; }
      duelRender();
      return;
    }
    if (m.type === 'joined' || m.type === 'left') {
      const have = new Set(m.players || []);
      for (const n of Object.keys(DUEL.progress)) if (!have.has(n)) delete DUEL.progress[n];
      for (const n of have) if (n !== DUEL.name) DUEL.progress[n] ??= { clicks: 0 };
      duelRender();
      return;
    }
    if (m.name && m.name !== DUEL.name) {
      const p = DUEL.progress[m.name] ??= { clicks: 0 };
      if (m.type === 'click') p.clicks = m.clicks;
      if (m.type === 'finish') { p.clicks = m.clicks; p.done = true; }
      duelRender();
    }
  };
  ws.onclose = () => { if (DUEL && DUEL.ws === ws) { DUEL = null; $('duelbar').innerHTML = ''; } };
}
function duelSend(frame) {
  if (DUEL && DUEL.ws.readyState === 1) {
    try { DUEL.ws.send(JSON.stringify(frame)); } catch {}
  }
}
$('duelnew').onclick = async () => {
  const name = $('duelname').value.trim() || 'player 1';
  $('duelerr').textContent = '';
  try {
    const r = await api('/api/duel/new', { method: 'POST' });
    $('duelcode').value = r.code;
    duelConnect(r.code, name);
  } catch (e) { $('duelerr').textContent = e.message; }
};
$('dueljoin').onclick = () => {
  const code = $('duelcode').value.trim().toUpperCase();
  if (code.length !== 6) { $('duelerr').textContent = 'room codes are 6 characters'; return; }
  $('duelerr').textContent = '';
  duelConnect(code, $('duelname').value.trim() || 'player 2');
};
$('duelcopy').onclick = () => duelCopy($('duelcode').value.trim().toUpperCase(), $('duelcopy'));
$('duelbar').onclick = e => {
  const chip = e.target.closest('.chip.copy');
  if (chip && DUEL) duelCopy(DUEL.code, chip);
};

