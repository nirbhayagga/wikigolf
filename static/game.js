/* ---------------- game ---------------- */
const dist = (a,b) => (a&&b&&a.x!=null&&b.x!=null) ? Math.hypot(a.x-b.x,a.y-b.y) : null;

function startClock() { state.started = performance.now(); runClock(); }
function stopClock(){ if (TICK) { clearInterval(TICK); TICK=null; } }

/* ---------------- the veil ----------------
   One mechanism for two states that need the same thing: the clock stopped and
   the links genuinely out of sight.

   'ready'  — a race is loaded but not begun. The daily used to start its clock
              during page load, so your first recorded seconds were spent
              waiting for the map to render.
   'paused' — mid-race.

   Hiding the links is the substance, not decoration. A pause that leaves the
   list on screen is not a pause: you can read every option, plan three moves,
   and resume with the clock none the wiser. */
function setVeil(mode) {          // 'ready' | 'paused' | null
  state.veil = mode;
  $('veil').style.display = mode ? 'grid' : 'none';
  $('links').inert = !!mode || state.done;
  $('linkbar').inert = !!mode || state.done;
  $('pause').textContent = mode === 'paused' ? 'Resume' : 'Pause';
  $('pause').disabled = !state.goal || state.done || mode === 'ready';
  updateCompass();
  $('back').disabled = state.done || !!mode || state.trail.length < 2;
  $('giveup').disabled = !!mode || !state.goal || state.done;
  // The reader is a veil leak: article text contains the links. No reading
  // while the clock isn't running.
  $('readbtn').disabled = !!mode || !state.cur || state.done;
  if (mode) closeReader();
  // Search and the custom-race pickers stay live under the veil. They were
  // disabled here at first, which meant they were dead on page load — the
  // daily arms itself immediately, so 'ready' greyed them out before the
  // player had touched anything. They are also not a way around a pause:
  // search returns titles and in-degrees, never an article's links, and the
  // custom form abandons the current race rather than advancing it.
  if (mode === 'ready') {
    $('veiltitle').textContent = 'Ready when you are';
    $('veilsub').innerHTML = `Get to <b>${esc(state.goal ? state.goal.title : '')}</b>` +
      (state.par != null ? ` in <b>${state.par}</b> clicks` : '');
    $('veilbtn').textContent = 'Start race';
  } else if (mode === 'paused') {
    $('veiltitle').textContent = 'Paused';
    $('veilsub').textContent = 'The clock is stopped and the links are hidden.';
    $('veilbtn').textContent = 'Resume';
  }
}

function runClock() {
  stopClock();
  TICK = setInterval(() => {
    if (!state.done && !state.veil) { $('time').textContent = mmss(performance.now()-state.started); saveProgress(); }
  }, 500);
}

let CLOCK_BUSY = false, PUZZLE_LOADING = false;
async function clockAction(action, path = []) {
  if (STATIC || state.practice) return null;
  try {
    const r = await api('/api/clock', {method:'POST', headers:{'content-type':'application/json'},
      body:JSON.stringify({run:state.run, action, path})});
    return r.ms;
  } catch (e) {
    state.practice = true;
    announce(`Practice mode: ${e.message}. Your route still counts locally.`);
    return null;
  }
}

async function unveil() {
  if (CLOCK_BUSY || NAV || PUZZLE_LOADING) return;
  CLOCK_BUSY = true;
  const ms = await clockAction(state.veil === 'ready' ? 'start' : 'resume');
  if (state.veil === 'ready') {
    state.started = performance.now();   // the race begins here, not at page load
  } else if (state.veil === 'paused') {
    // Shift the start forward by the time spent paused, so elapsed stays right
    // without a second accumulator to keep in step.
    state.started += performance.now() - state.pauseAt;
  }
  if (ms != null) state.started = performance.now() - ms;
  setVeil(null);
  runClock();
  CLOCK_BUSY = false;
  saveProgress();
  $('lfilter').focus();
}

$('veilbtn').onclick = unveil;
$('pause').onclick = async () => {
  if (CLOCK_BUSY || NAV || PUZZLE_LOADING) return;
  if (state.veil === 'paused') { unveil(); return; }
  if (!state.goal || state.done || state.veil) return;
  state.pauseAt = performance.now();
  stopClock();
  setVeil('paused');
  CLOCK_BUSY = true;
  const ms = await clockAction('pause');
  if (ms != null) { state.started = state.pauseAt - ms; $('time').textContent = mmss(ms); }
  CLOCK_BUSY = false;
  saveProgress();
};

// One navigation at a time. A lagged double-click used to fire go() twice
// before the first fetch resolved, adding the article to the trail twice and
// charging two clicks for one decision. The same-article guard covers the
// second click landing after the first resolved.
let NAV = false;
async function go(id, isBack, initial = false) {
  if (!initial && (state.veil || state.done || PUZZLE_LOADING)) return;
  if (NAV) return;
  if (!isBack && state.cur && state.cur.id === id) return;
  NAV = true;
  try { return await goInner(id, isBack); } finally { NAV = false; }
}

async function goInner(id, isBack) {
  const navigatingState = state;
  const q = new URLSearchParams();
  if (state.ban != null) q.set('ban_degree', state.ban);
  if (state.goal) q.set('goal', state.goal.id);
  const d = await api(`/api/article/${id}?${q}`);
  if (state !== navigatingState) return;

  if (!isBack) {
    // One entry per click. Deliberately not "did this get closer on the map" —
    // that was measured at a 1.18x lift over guessing, so the green and red
    // squares were decorating noise. Until the goal-distance BFS exists, the
    // only true thing we can say per click is whether it was still inside par.
    if (state.cur) state.steps.push(state.trail.length <= (state.par ?? 0));
    state.trail.push(d);
  }
  state.cur = d;

  const clicks = Math.max(0, state.trail.length-1);
  $('cur').textContent = d.title;
  $('readbtn').disabled = !!state.veil || state.done;
  $('curmeta').textContent =
    `${d.links.length.toLocaleString()} links out · ${d.in_degree.toLocaleString()} in` +
    // Article size: a stub and a 600 KB monster are very different places to
    // be standing, and the dump gives the number away for free.
    (d.bytes ? ` · ${d.bytes >= 1024 ? Math.round(d.bytes/1024)+' KB' : d.bytes+' B'}` : '') +
    // What people actually read — the fame signal players recognise.
    (d.views ? ` · ${fmtViews(d.views)} reads/mo` : '');
  $('curcats').textContent = d.desc || (d.cats || []).join(' · ');
  $('curmeta').hidden = !$('articleinfo').open;
  $('curalias').hidden = !$('articleinfo').open;
  renderJourney();
  $('curalias').textContent = (d.aliases || []).length
    ? 'also: ' + d.aliases.join(', ') : '';
  $('clicks').textContent = clicks;
  $('clicks').className = state.par!=null && clicks>state.par ? 'par-over' : 'par-ok';
  if (!state.done) duelSend({ type: 'click', at: d.title, clicks });
  $('trail').innerHTML = state.trail.map(a=>`<b>${esc(a.title)}</b>`).join(' &rsaquo; ');
  $('trail').scrollTop = 1e6;
  // !!state.veil: this runs on every navigation, and must not re-enable the
  // controls the veil deliberately disabled.
  $('back').disabled = state.done || !!state.veil || state.trail.length < 2;
  $('giveup').disabled = !!state.veil || !state.goal || state.done;
  $('pause').disabled = !!state.veil || !state.goal || state.done;
  updateCompass();

  if (state.goal && d.id === state.goal.id) return finish(true);
  renderLinks(d);
  saveProgress();
  const focus = state.trail.filter(a=>a.x!=null);
  if (state.goal && state.goal.x!=null) focus.push(state.goal);
  fitTo(focus);
}

// Links of the current article, kept so filtering and re-sorting are local
// operations — a busy article has hundreds of links, and none of this needs
// the server.
let CURLINKS = [];
const COLLAPSED = new Set();

function renderLinks(d) {
  CURLINKS = d.links;
  // Readings belong to the article you were standing on. Carrying them
  // forward would turn one charge into a hint for the whole race.
  state.dist = null;
  state.hintEmpty = false;
  updateCompass();
  $('lfilter').value = '';
  paintLinks();
}

function paintLinks() {
  const box = $('links');
  if (!CURLINKS.length) {
    $('linkbar').style.display = 'none';
    box.innerHTML = `<div class="empty">Dead end &mdash; nothing links out of here.<br>
      Use <b>&larr; Back</b> to return.</div>`;
    return;
  }
  $('linkbar').style.display = 'flex';

  const q = $('lfilter').value.trim().toLowerCase();
  const mode = $('lsort').value;
  const links = CURLINKS.filter(l => !q || l.title.toLowerCase().includes(q));

  // Banned links sink regardless of sort: they cannot be taken, so they are
  // never what you are looking for.
  links.sort((a,b) => {
    if (!!a.banned !== !!b.banned) return a.banned ? 1 : -1;
    // A spent compass charge bought exact distances; closest-first is the
    // full delivery of that purchase. (The OLD distance sort was removed
    // for being map-distance guesswork — this one is graph truth.)
    if (state.dist) {
      const da = state.dist[a.id] ?? 99, db = state.dist[b.id] ?? 99;
      if (da !== db) return da - db;
      return b.in_degree - a.in_degree;
    }
    if (mode === 'az')  return a.title.localeCompare(b.title);
    if (mode === 'za')  return b.title.localeCompare(a.title);
    if (mode === 'asc') return a.in_degree-b.in_degree;
    // Missing views (no pageviews.parquet yet) sort as zero, which makes
    // this order identical-looking to a broken one — hence the meta flag
    // could gate the option, but a harmless no-op beats a hidden control.
    if (mode === 'read') return (b.views||0)-(a.views||0);
    return b.in_degree-a.in_degree;
  });

  $('lcount').textContent = q
    ? `${links.length} of ${CURLINKS.length}`
    : `${CURLINKS.length} link${CURLINKS.length===1?'':'s'}`;

  if (!links.length) {
    box.innerHTML = `<div class="empty">No link here matches &ldquo;${esc(q)}&rdquo;.</div>`;
    return;
  }

  // Sorted by in-degree, NOT by distance to the goal on the map.
  //
  // Map distance was measured against ground truth (reverse BFS from the goal,
  // 760,506 links across six goals) and predicts a real step towards the goal
  // 27.5% of the time against a 23.3% base rate — a 1.18x lift. Articles with
  // real force-layout positions score 1.23x, so a better layout does not
  // rescue it: a 2D embedding of a 7.2M-node graph with effective diameter ~4
  // simply cannot encode graph distance.
  //
  // The ordering mattered as much as the removed badge did. Players read list
  // order as importance whether or not there is an arrow beside it, so sorting
  // by that metric was the same lie in a quieter voice. In-degree is at least
  // a true fact about the graph.
  const frag = document.createDocumentFragment();

  // Grouped by map region, because a flat list of several hundred links is the
  // hardest part of this game. Real wiki-racing works because links sit inside
  // sentences that say what they are; stripping the prose removes exactly that
  // signal. The regions are the next best thing we already have, and they are
  // the same ones the map is coloured by — so the list teaches the map.
  //
  // An explicit sort choice wins over grouping: inside region buckets an A→Z
  // or most-read order is invisible, so honouring both at once honours
  // neither. Groups return when the sort goes back to its default.
  if (state.group && !q && !state.dist && mode === 'deg') {
    const by = new Map();
    for (const l of links) {
      const c = l.community ?? null;
      if (!by.has(c)) by.set(c, []);
      by.get(c).push(l);
    }
    // A 185-link list scattered over dozens of 1-2 link regions is worse
    // than no grouping at all. Small groups pool into one bucket at the end;
    // real regions keep their headers.
    let groups = [...by.entries()].sort((a, b) => b[1].length - a[1].length);
    const small = groups.filter(([, items]) => items.length < 3);
    if (small.length > 1) {
      groups = groups.filter(([, items]) => items.length >= 3);
      groups.push(['__misc', small.flatMap(([, items]) => items)]);
    }
    for (const [c, items] of groups) {
      const h = document.createElement('button');
      h.className = 'ghead';
      const open = !COLLAPSED.has(c);
      h.innerHTML =
        `<span class="gdot" style="background:${c === '__misc' ? 'var(--locked)' : colorOf(c)}"></span>` +
        `<span class="gname">${c === '__misc' ? 'Elsewhere on the map' : esc(regionName(c))}</span>` +
        `<span class="tag">${items.length}</span>` +
        `<span class="gcar">${open ? '\u25be' : '\u25b8'}</span>`;
      h.onclick = () => {
        COLLAPSED.has(c) ? COLLAPSED.delete(c) : COLLAPSED.add(c);
        paintLinks();
      };
      frag.appendChild(h);
      if (open) for (const l of items) frag.appendChild(linkRow(l));
    }
    box.innerHTML = ''; box.appendChild(frag); box.scrollTop = 0;
    return;
  }

  for (const l of links) {
    frag.appendChild(linkRow(l));
  }
  box.innerHTML=''; box.appendChild(frag); box.scrollTop=0;
}

/// One row. Shared by the flat and grouped renderings so they cannot drift.
///
/// Noise budget: a row shows at most the name, one context line, and two
/// tags. Extra data earns its place contextually instead of piling on —
/// read counts appear only when sorting by Most read (they are the sort
/// key, so hiding them would make the order look arbitrary), and the
/// details toggle strips rows to bare names for players who know the
/// territory. The compass and banned tags always show: they are gameplay,
/// not decoration.
function linkRow(l) {
    const el = document.createElement('button');
    const detail = $('ldetails').checked;
    const byReads = $('lsort').value === 'read';
    el.className='link'; el.disabled = !!l.banned;
    if (l.banned) el.title = `Banned at this difficulty: ${l.in_degree.toLocaleString()} inbound links`;
    el.innerHTML =
      ``+
      `<span class="name">${esc(l.title)}` +
        // The context a bare link list otherwise has none of. The editor's
        // own {{Short description}} beats categories when the v3 parse has
        // run; categories are the fallback. One line either way — the noise
        // budget does not grow.
        (detail && (l.desc || (l.cats && l.cats.length))
          ? `<span class="cats">${esc(l.desc || l.cats.slice(0,3).join(' · '))}</span>` : '') +
      `</span>`+
      (l.disambig ? `<span class="tag">fork</span>` : '')+
      (l.banned ? `<span class="tag lock">blocked hub</span>` : '')+
      (state.dist && state.dist[l.id] !== undefined
        ? `<span class="tag ${state.dist[l.id] != null && state.dist[l.id] <= 1 ? 'near' : 'far'}">`+
          (state.dist[l.id] === null ? `&gt;${state.distDepth || 6}` : `${state.distExact ? '' : '≥'}${state.dist[l.id]} to go`)+
          `</span>` : '')+
      (byReads
        ? `<span class="tag">${fmtViews(l.views||0)} reads</span>`
        : (detail ? `<span class="tag">${l.in_degree.toLocaleString()}</span>` : ''));
    if (!l.banned) el.onclick = () => go(l.id);
    return el;
}

function routeHtml(path, mine) {
  const set = new Set(mine.map(a=>a.id));
  return path.map((a,i) =>
    `<div class="step${set.has(a.id)?' hit':''}"><div class="n">${i}</div>`+
    `<div class="t">${esc(a.title)}</div></div>`).join('');
}

async function finish(won) {
  state.done = true;
  state.elapsed = state.started ? performance.now()-state.started : 0;
  if (won && !state.jumped) {
    const ms = await clockAction('finish', state.trail.map(a => a.id));
    if (ms != null) state.elapsed = ms;
  }
  try { localStorage.removeItem('wr-progress'); } catch {}
  stopClock();
  setVeil(null);
  $('giveup').disabled = true;
  $('pause').disabled = true;
  updateCompass();
  const clicks = state.trail.length-1, par = state.par;
  if (won) duelSend({ type: 'finish', clicks, ms: Math.round(state.elapsed) });

  // Golf terms, because the game is golf. Par is the optimal route, so
  // under-par cannot exist honestly — a run that shows it was jumped, and a
  // jumped run is not scored at all.
  const golf = clicks===par ? 'Par ⛳' : clicks===par+1 ? 'Bogey'
    : clicks===par+2 ? 'Double bogey' : clicks>par ? `+${clicks-par} over par`
    : `${par-clicks} under par?!`;
  const scored = won && !state.jumped;
  $('verdict').textContent = !won ? 'Gave up'
    : state.jumped ? 'Found it — unscored (search jump)' : golf;
  $('score').innerHTML = won
    ? `<b>${clicks}</b> click${clicks===1?'':'s'} &nbsp;·&nbsp; par <b>${par}</b> &nbsp;·&nbsp; <b>${mmss(state.elapsed)}</b>`
    : `par was <b>${par}</b>`;

  // Daily streak, tracked on this device: consecutive daily numbers won.
  // localStorage rather than the server, so it needs no account and costs
  // nothing — the tradeoff is that clearing the browser clears the streak.
  let streak = 0;
  if (scored && state.number != null && !state.archive && !ROUND) {
    let s = {};
    try { s = JSON.parse(localStorage.getItem('wr-streak')) || {}; } catch {}
    if (s.last !== state.number) {
      s.streak = (s.last === state.number - 1) ? (s.streak || 0) + 1 : 1;
      s.last = state.number;
      s.best = Math.max(s.best || 0, s.streak);
      localStorage.setItem('wr-streak', JSON.stringify(s));
    }
    streak = s.streak || 0;
    updateStreakChip();
    if (streak > 1) $('verdict').textContent += ` · 🔥${streak}`;
  }

  // Green while inside par, red for every click past it. On the contrast
  // theme the squares go blue/orange instead (Wordle's own high-contrast
  // answer): the squares are the one place colour is the only channel, and
  // emoji can't follow CSS variables.
  const hc = localStorage.getItem('wr-theme') === 'contrast';
  const sq = state.steps.map(s => s ? (hc?'🟦':'🟩') : (hc?'🟧':'🟥'));
  $('squares').textContent = scored ? sq.join('') : '';
  state.share = (state.number?`WikiGolf #${state.number}`:'WikiGolf')+
    ` (${state.difficulty})`+(streak>1?` · 🔥${streak}-day streak`:'')+`\n`+
    (scored ? `${golf} — ${clicks} click${clicks===1?'':'s'}, par ${par} · ${mmss(state.elapsed)}\n${sq.join('')}`
      : won ? `found it after a search jump — unscored · par ${par}`
      : `gave up · par ${par}`)+
    (state.url ? `\n${state.url}` : '');

  // Only a completed, unjumped run can be submitted — and even then only
  // the server decides whether it counts (it re-walks the path).
  $('post').disabled = !scored || state.practice;
  if (state.practice) $('posted').textContent = 'Practice run — not ranked.';
  loadBoard();
  $('posted').textContent = state.practice ? 'Practice run — not ranked.' : '';
  $('nick').value = localStorage.getItem('wr-nick') || '';

  $('routetxt').textContent = 'A shortest route';
  $('replay').style.display = 'none';
  $('routes').textContent = 'counting routes…';
  $('route').innerHTML = '<div class="empty">finding&hellip;</div>';
  // No mode name here: medium and hard both ban hubs (at different levels),
  // so naming one mislabels the other — state the rule, not a label.
  $('note').textContent = state.ban
    ? `Routes through articles with more than ${state.ban.toLocaleString()} inbound links were banned for both you and the optimal route.`
    : '';
  openOverlay('result');
  if (STATIC) $('dailydist').textContent = '';
  if (ROUND) {
    // A jumped hole is a pickup: you didn't play it from the tee.
    roundHoleDone(scored, clicks, par);
    if (ROUND.idx + 1 < ROUND.holes.length) {
      // Mid-round: the verdict is the hole, the button is the next tee.
      $('verdict').textContent += `  ·  hole ${ROUND.idx+1}/${ROUND.holes.length}`;
      $('nexthole').style.display = '';
    } else {
      roundScorecard();
    }
  } else if (state.number != null) {
    showDist();
    if (!state.archive) startCountdown();
  }

  if (STATIC) {
    // No pathfinder without a server: the route was baked into the puzzle
    // file at export time ([[id,title,x,y],...]). The from-here analysis
    // stays live-only. An old cached puzzle without one just leaves the
    // box hidden — never show "no route exists" for routes that exist.
    $('fromherebox').style.display = 'none';
    $('route').innerHTML = ''; $('routetxt').textContent = '';
    const p = (state.route || []).map(([id, title, x, y]) => ({ id, title, x, y }));
    if (p.length > 1) {
      $('route').innerHTML = routeHtml(p, state.trail);
      $('routetxt').textContent = `A shortest route (${p.length - 1} clicks)`;
      state.best = p;
      if (p.filter(a => a.x != null).length > 1) $('replay').style.display = '';
    }
    draw();
  }
  const ask = (from) => api('/api/path', { method:'POST',
    headers:{'content-type':'application/json'},
    body: JSON.stringify({ from, to: state.goal.id, ban_degree: state.ban }) });

  try {
    // Static already filled the route box from the baked route above; the
    // throw skips the live lookup, and the catch below must not wipe it.
    if (STATIC) throw new Error('static');
    const p = await ask(state.trail[0].id);
    if (p.found) {
      // "A" shortest route, not "the": ties are common in a graph this dense
      // and we return whichever the bidirectional search met on.
      $('route').innerHTML = routeHtml(p.path, state.trail);
      $('routetxt').textContent = `A shortest route (${p.clicks} clicks)`;
      state.best = p.path;
      // Only offer the replay when the route can actually be drawn.
      if (p.path.filter(a => a.x != null).length > 1) $('replay').style.display = '';
      draw();
    } else $('route').innerHTML = '<div class="empty">no route exists</div>';
  } catch { if (!STATIC) $('route').innerHTML=''; }

  // On a give-up, the route from the start is the least interesting answer —
  // you want to know what you should have done from where you actually got to.
  // Skipped when you never moved, since it would just repeat the block above.
  // Live-only: static has no pathfinder, and unguarded this box unhid itself
  // and printed "could not check" on every static give-up.
  const here = state.trail[state.trail.length-1];
  if (!STATIC && !won && here && here.id !== state.trail[0].id) {
    $('fromherebox').style.display = '';
    $('fromherelbl').textContent = `From ${here.title}`;
    $('fromhere').innerHTML = '<div class="empty">finding&hellip;</div>';
    try {
      const q = await ask(here.id);
      $('fromherelbl').textContent = q.found
        ? `From ${here.title} it was ${q.clicks} more click${q.clicks===1?'':'s'}`
        : `From ${here.title} there was no way through`;
      $('fromhere').innerHTML = q.found
        ? routeHtml(q.path, [])
        : `<div class="empty">Nothing links onward to the goal from here` +
          (state.ban ? ` at this hub level.` : `.`) +
          ` Backing up was the only move.</div>`;
    } catch { $('fromhere').innerHTML = '<div class="empty">could not check</div>'; }
  } else {
    $('fromherebox').style.display = 'none';
  }
  // How many optimal routes existed. Asked now rather than at generation:
  // counting is a full BFS, and the player is already finished.
  try {
    const r = await apiRun(() => api(`/api/routes?run=${state.run}`));
    const n = r.routes;
    $('routes').innerHTML = n === 1
      ? `There was exactly <b>one</b> shortest route.`
      : `There were <b>${r.saturated ? 'over 18 quintillion' : n.toLocaleString()}</b> `+
        `different ${r.clicks}-click routes.`;
  } catch { $('routes').textContent = ''; }
  draw();
}

// Slider stops. Not linear: the interesting range is all at the top of the
// in-degree distribution, so each step is roughly a quarter-order of magnitude.
const HUB_STOPS = [0, 25, 100, 500, 2500, 10000];

async function describeHubs() {
  const n = HUB_STOPS[+$('hubs').value];
  $('hublvl').textContent = n === 0 ? 'off' : `top ${n.toLocaleString()}`;
  if (n === 0) {
    $('hubwhat').innerHTML = 'Every link is playable, including the mega-hubs. Most pairs are 2&ndash;3 clicks.';
    return;
  }
  try {
    const d = await api('/api/hubs?n=' + n);
    const names = d.sample.slice(0, 6).map(x => `<b>${esc(x.title)}</b>`).join(', ');
    $('hubwhat').innerHTML =
      `Blocking <b>${d.excluded.toLocaleString()}</b> articles with more than ` +
      `<b>${d.ban_degree.toLocaleString()}</b> inbound links &mdash; ${names}&hellip;`;
  } catch { $('hubwhat').textContent = ''; }
}

async function startRace(endpoint, extra) {
  if (NAV || CLOCK_BUSY || PUZZLE_LOADING) { announce("Finish the current action first."); return; }
  PUZZLE_LOADING = true;
  for (const b of ['new','daily']) $(b).disabled = true;
  closeOverlay('result');
  try {
    // The daily is a fixed preset so everyone races the same thing; the
    // slider only steers free play.
    const isDaily = endpoint.endsWith('daily');
    setMode(isDaily ? 'daily' : 'random');
    // Each difficulty has its own daily (the server mixes difficulty into
    // the seed), so the picker beside the button is a real choice, not a
    // reroll of the same race.
    const anum = $('dnum').value.trim();
    const q = extra ? (new URLSearchParams(extra).has('ban_degree') || new URLSearchParams(extra).has('ban_top') ? extra : `${extra}&ban_top=${HUB_STOPS[+$('hubs').value]}`)
      : isDaily
      ? `difficulty=${$('ddiff').value}${anum ? `&number=${encodeURIComponent(anum)}` : ''}`
      : STATIC
      ? `difficulty=${$('rdiff').value}${$('rpar').value ? `&par=${$('rpar').value}` : ''}`
      : `difficulty=hard&ban_top=${HUB_STOPS[+$('hubs').value]}`;
    const diff = isDaily ? 'daily' : 'custom';
    const p = await api(`${endpoint}?${q}`);
    ROUND = null; // starting any single race abandons an in-progress round
    await adoptPuzzle(p, { archive: isDaily && !!anum && Number(anum) !== todayNumber(), difficulty: diff });
  } catch(e) {
    announce(e.message);
    // Rethrown so a caller that has somewhere better to put the message — the
    // custom-race form, which can suggest lowering the hub level — can use it.
    // Callers with nowhere to show it swallow it explicitly.
    throw e;
  } finally { PUZZLE_LOADING = false; for (const b of ['new','daily']) $(b).disabled = false; }
}

/* Load one issued puzzle into play. Shared by single races and course
   holes so the two can never drift: a hole IS a race, with the compass,
   the map, the veil and the clock all behaving identically. */
async function adoptPuzzle(p, opts) {
  stopClock();
  // Starting any race that isn't the duel's own board leaves the room.
  if (DUEL && !DUEL.adopting) duelStop();
  state = blank();
  stopDailyTicker();
  $('nexthole').style.display = 'none';
  Object.assign(state, {
    archive: !!opts.archive, puzzle:p, practice:false,
    goal:p.goal, par:p.optimal, ban:p.ban_degree, run:p.run,
    startId:p.start.id, route:p.route??null,
    number:p.number??null, difficulty:p.difficulty||opts.difficulty, compass:p.compass??0 });
  $('goal').textContent = p.goal.title;
  $('goalmeta').textContent = [p.goal.desc,
    ''
  ].filter(Boolean).join(' · ');
  $('par').textContent = p.optimal;
  $('time').textContent = '0:00';
  $('pnum').textContent = ROUND
    ? `Round #${ROUND.number} · hole ${ROUND.idx+1}/${ROUND.holes.length}`
    : (p.number ? `#${p.number}` : '');
  setRaceUrl(p);
  $('chal').style.display = '';
  await go(p.start.id, false, true);
  // Armed, not started. setVeil('ready') holds the clock at 0:00 until the
  // player says go.
  setVeil('ready');
}

/* ---------------- the daily round ----------------
   Nine holes, one designed par profile, one scorecard. ROUND survives
   state = blank() on purpose: it is the thing that spans races. */
let ROUND = null;

async function startRound() {
  if (NAV || CLOCK_BUSY || PUZZLE_LOADING) return;
  PUZZLE_LOADING = true;
  setMode('round');
  for (const b of ['new','daily','round']) $(b).disabled = true;
  closeOverlay('result');
  try {
    const anum = $('dnum').value.trim();
    const qp = new URLSearchParams({ holes: $('rholes').value });
    if (anum) qp.set('number', anum);
    const c = await api(`/api/course?${qp}`);
    ROUND = { number: c.number, par: c.par, holes: c.holes, idx: 0,
              strokes: [], pars: [], archive: !!anum && Number(anum) !== todayNumber() };
    await adoptPuzzle(c.holes[0], { archive: ROUND.archive, difficulty: 'round' });
  } catch(e) {
    announce(e.message);
  } finally { PUZZLE_LOADING = false; for (const b of ['new','daily','round']) $(b).disabled = false; }
}

/* Record the finished hole. A give-up is a "pickup" at par+3, golf's
   informal mercy rule — a round must always be finishable. */
function roundHoleDone(won, clicks, par) {
  ROUND.strokes.push(won ? clicks : par + 3);
  ROUND.pars.push(par);
}

function roundScorecard() {
  const total = ROUND.strokes.reduce((a,b)=>a+b, 0);
  const parTotal = ROUND.pars.reduce((a,b)=>a+b, 0);
  const overall = total - parTotal;
  const rel = ROUND.strokes.map((s,i) => s - ROUND.pars[i]);
  const marks = rel.map(d => d<=0 ? '\u26F3' : d===1 ? '\u{1F7E8}' : '\u{1F7E5}').join('');
  $('verdict').textContent =
    `Round ${overall<=0 ? 'at par! \u26F3' : `+${overall}`} — ${total} strokes`;
  $('score').innerHTML =
    `course par <b>${parTotal}</b> · holes: ` +
    ROUND.strokes.map((s,i)=>`${s}/${ROUND.pars[i]}`).join(' · ');
  $('squares').textContent = marks;
  state.share = `WikiGolf Round #${ROUND.number}` +
    ` — ${total} strokes, par ${parTotal} (${overall<=0?'at par \u26F3':'+'+overall})\n` +
    marks + `\n${state.url}`;
  ROUND = null;
}

/* ---------------- leaderboard ----------------
   Boards are keyed by exactly what was played: free:<difficulty> for free
   play, daily:<number>:<difficulty> for the daily. The client holds both
   values already, so it asks for its own board rather than relying on the
   endpoint's default — which is "medium", a board normal play never writes
   to, and which therefore always came back empty. */
async function loadBoard() {
  const q = new URLSearchParams({ difficulty: state.difficulty || 'hard' });
  if (state.number != null) q.set('number', state.number);
  $('boardname').textContent = state.number != null
    ? `Daily #${state.number}` : `free play, ${state.difficulty || 'hard'}`;
  try {
    const rows = await api('/api/leaderboard?' + q);
    $('board').innerHTML = rows.length
      ? rows.map(r =>
          `<div class="brow"><span class="n">${r.rank}</span>` +
          `<span class="who">${esc(r.nickname || 'anonymous')}</span>` +
          `<span class="tag">${r.clicks} clicks</span>` +
          `<span class="tag">${mmss(r.ms)}</span></div>`).join('')
      : `<div class="empty">No scores posted here yet.</div>`;
  } catch (e) {
    $('board').innerHTML = `<div class="empty">${esc(e.message || 'could not load')}</div>`;
  }
}

/* ---------------- theme + help ---------------- */
function setTheme(name) {
  document.documentElement.setAttribute('data-theme', name);
  $('theme').value = name;
  try { localStorage.setItem('wr-theme', name); } catch {}
  // The canvas paints with plain strings, not CSS, so it has to be told.
  readTheme(); requestMap();
}
$('theme').addEventListener('change', e => setTheme(e.target.value));

function showHelp(on) {
  if(on) openOverlay('help'); else closeOverlay('help');
  if (on) $('hnever').checked = localStorage.getItem('wr-help') === 'off';
}
$('helpbtn').onclick = () => showHelp(true);
$('helpclose').onclick = () => {
  // The preference is recorded on close rather than on tick, so changing your
  // mind before closing does what it looks like it does.
  localStorage.setItem('wr-help', $('hnever').checked ? 'off' : 'on');
  showHelp(false);
};

/* ---------------- compass ----------------
   The honest replacement for the map-distance arrows. Those compared
   straight-line positions and scored a 1.18x lift over guessing; these are the
   real shortest-path distance, computed by a reverse BFS from the goal.

   Rationed because they are exact: shown on every article for free you would
   simply click the smallest number each turn. Charges scale with par — one on
   a three-click race, four on a six — so you can always check about half your
   moves and never all of them. The server owns the count; the client is only
   allowed to display it. */
let HINT_BUSY = false;
function updateCompass() {
  const b = $('compass');
  const live = state.goal && !state.done && !state.veil;
  b.disabled = HINT_BUSY || !live || state.compass <= 0 || !!state.dist || state.hintEmpty;
  // Static: compass-lite covers dailies and rounds in the exported window.
  // Races without data hide the button rather than showing a dead control.
  if (STATIC) b.style.display = (state.compass > 0 || state.dist) ? '' : 'none';
  b.textContent = state.hintEmpty ? `Hint saved · ${state.compass} left` : state.dist ? 'Hint shown'
    : `Hint${state.compass ? ' · ' + state.compass + ' left' : ''}`;
}

$('compass').onclick = async () => {
  if (HINT_BUSY || PUZZLE_LOADING || NAV || !state.goal || state.done || state.veil || state.compass <= 0 || state.dist || state.hintEmpty) return;
  HINT_BUSY = true;
  const hintState = state, hintArticle = state.cur.id;
  $('compass').disabled = true;
  try {
    const prev = state.compass;
    // The body is rebuilt per attempt: after a reissue, state.run is new.
    const r = await apiRun(() => api('/api/compass', { method:'POST',
      headers:{'content-type':'application/json'},
      body: JSON.stringify({ run: state.run, from: state.cur.id }) }));
    // A re-issued run has a full set of charges server-side; the honest count
    // is one less than this tab had, whichever is smaller.
    if (state !== hintState) return;
    state.compass = Math.min(r.charges_left, Math.max(0, prev - (r.charged === false ? 0 : 1)));
    if (state.cur.id !== hintArticle) { updateCompass(); saveProgress(); return; }
    if (r.charged === false) {
      state.hintEmpty = true;
      $('links').insertAdjacentHTML('afterbegin', '<div class="compassnote" role="status">No measured distances for your playable links here. Your hint was saved—try it after moving to another article.</div>');
      return;
    }
    // Compass-lite files may cover fewer than 6 levels; ">n" must tell the
    // truth about which n. The live server's cap is 6 and sends no depth.
    state.distDepth = r.depth || 6;
    state.distExact = r.exact !== false || state.ban == null;
    state.dist = {};
    for (const l of r.links) state.dist[l.id] = l.dist;
    paintLinks();
    // The distances land as small tags, which playtesting called too subtle
    // to notice — so say what just happened, loudly and specifically.
    const dists = CURLINKS.filter(l => !l.banned).map(l => state.dist[l.id]).filter(d => d != null);
    const best = dists.length ? Math.min(...dists) : null;
    const nBest = dists.filter(d => d === best).length;
    $('links').insertAdjacentHTML('afterbegin',
      `<div class="compassnote" role="status">${state.distExact ? 'Hint: shortest distances to the goal.' : 'Hint: lower bounds. These distances may use blocked hubs; your legal route can be longer.'} Closest first${
        best != null ? ` — ${nBest} link${nBest===1?' is':'s are'} ${state.distExact ? '' : 'at least '}${best} away` : ''}</div>`);
  } catch (e) {
    // A successful server reading can still cost a charge if its response is lost.
    if (state === hintState && state.cur.id === hintArticle) $('links').insertAdjacentHTML('afterbegin',
      `<div class="empty">${esc(e.message || 'compass failed')}</div>`);
  } finally {
    HINT_BUSY = false;
    updateCompass(); saveProgress();
  }
};

/* ---------------- shareable races ----------------
   The address bar is the share link: every race rewrites it, so copying the
   URL is always enough to hand someone the same race.

   Titles, not ids. Article ids are dense 0..N-1 in parse order, so the next
   dump shifts every one of them and an id-based link would quietly start
   pointing at a different article — a link that breaks silently is worse than
   one that breaks loudly. Titles survive a refresh, and the server resolves
   them through redirects.

   The daily gets a clean URL instead: it is defined by the day, so pinning
   its endpoints would hand someone yesterday's race. */
function raceUrl(p) { return RaceLink.encode(location.href, p, META, ROUND); }

function setRaceUrl(p) {
  state.url = raceUrl(p);
  // replaceState, not pushState: a race is not a place you go Back to, and
  // filling the history with one entry per race would trap the player.
  history.replaceState(null, '', state.url);
}

async function copyText(text, btn, label) {
  try {
    // navigator.clipboard needs a secure context; a plain http:// LAN address
    // is not one, so fall back rather than failing silently.
    if (navigator.clipboard && window.isSecureContext) await navigator.clipboard.writeText(text);
    else { const ta=document.createElement('textarea'); ta.value=text;
           ta.style.cssText='position:fixed;opacity:0'; document.body.appendChild(ta);
           ta.select(); document.execCommand('copy'); ta.remove(); }
    btn.textContent='Copied!';
  } catch { btn.textContent='Press Ctrl+C'; }
  setTimeout(()=>{ btn.textContent=label; }, 1600);
}

$('copylink').onclick = () => copyText(state.url || location.href, $('copylink'), 'Copy race link');

/* ---------------- controls ---------------- */
// .catch(()=>{}): startRace already rendered the message into the link list,
// and an unhandled rejection here would only spam the console.
$('custom').addEventListener('toggle', () => { if($('custom').open && META?.has_map) loadMap().catch(()=>{}); });
$('topic').addEventListener('change', () => { $('tgo').disabled = !$('topic').value; });
$('tgo').onclick = () => {
  const c = $('topic').value;
  if (c === '') return;
  startRace('/api/puzzle', `region=${c}&difficulty=medium`)
    .catch(e => { $('cerr').textContent = e.message; });
};

$('round').onclick = () => startRound();
$('nexthole').onclick = async () => {
  $('nexthole').style.display = 'none';
  ROUND.idx += 1;
  await adoptPuzzle(ROUND.holes[ROUND.idx], { archive: ROUND.archive, difficulty: 'round' });
};

$('new').onclick   = () => startRace('/api/puzzle').catch(()=>{});
$('again').onclick = () => startRace('/api/puzzle').catch(()=>{});
$('daily').onclick = () => startRace('/api/daily').catch(()=>{});
$('giveup').onclick = () => { if (state.goal && !state.done && !state.veil && !NAV && !PUZZLE_LOADING) finish(false); };
$('back').onclick = () => {
  if (NAV || state.veil || state.done || state.trail.length < 2) return;
  state.trail.pop(); state.steps.pop();
  go(state.trail[state.trail.length-1].id, true);
};
$('zin').onclick  = () => { target.s *= 1.5; requestMap(); };
$('zout').onclick = () => { target.s /= 1.5; requestMap(); };
$('zfit').onclick = fitAll;
$('hubs').addEventListener('input', describeHubs);

cv.addEventListener('wheel', e => {
  e.preventDefault();
  target.s *= e.deltaY < 0 ? 1.12 : 1/1.12; requestMap();
}, { passive:false });
cv.addEventListener('pointerdown', e => { dragging={x:e.clientX,y:e.clientY}; cv.classList.add('drag'); cv.setPointerCapture(e.pointerId); });
cv.addEventListener('pointermove', e => {
  if (!dragging) return;
  const d = Math.min(window.devicePixelRatio||1,2);
  target.x -= (e.clientX-dragging.x)*d/cam.s;
  target.y += (e.clientY-dragging.y)*d/cam.s;
  cam.x = target.x; cam.y = target.y;      // drag should feel direct, not eased
  dragging = {x:e.clientX,y:e.clientY}; requestMap();
});
for (const ev of ['pointerup','pointercancel'])
  cv.addEventListener(ev, () => { dragging=null; cv.classList.remove('drag'); });

$('post').onclick = async () => {
  if (state.practice || !state.done) return;
  const nick = $('nick').value.trim();
  localStorage.setItem('wr-nick', nick);
  $('post').disabled = true;
  try {
    // The server is sent the route, not the score. It re-walks every hop on
    // its own graph and times the run on its own clock.
    const r = await api('/api/submit', { method:'POST',
      headers:{'content-type':'application/json'},
      body: JSON.stringify({ run: state.run, path: state.trail.map(a=>a.id), nickname: nick }) });
    $('posted').innerHTML = r.rank
      ? `Accepted &mdash; rank <b>#${r.rank}</b> on this board.`
      : 'Accepted.';
    loadBoard();
  } catch (e) {
    // A restart forgot the run: the race was real but its clock is gone, and
    // a time measured from the restart would be fake. Say what happened
    // instead of a cryptic rejection.
    $('posted').textContent = /unknown or expired/.test(e.message || '')
      ? 'The server restarted during this race, so its time can’t go on the leaderboard. The race itself still counts — rematch to post a time.'
      : `Rejected: ${e.message}`;
    $('post').disabled = false;
  }
};

/* The Wordle pair: how everyone else did, and when the next one lands.
   Both only for dailies; both cleared the moment a new race starts. */
let dailyTicker = null;
function stopDailyTicker() {
  if (dailyTicker) { clearInterval(dailyTicker); dailyTicker = null; }
  $('nextdaily').textContent = ''; $('dailydist').textContent = '';
}
function startCountdown() {
  const tick = () => {
    const left = 86400 - Math.floor(Date.now()/1000) % 86400;
    const h = Math.floor(left/3600), m = Math.floor(left%3600/60), s = left%60;
    $('nextdaily').textContent =
      `Next daily in ${h}:${String(m).padStart(2,'0')}:${String(s).padStart(2,'0')}`;
  };
  tick();
  dailyTicker = setInterval(tick, 1000);
}
async function showDist() {
  try {
    const d = await api(`/api/dist?number=${state.number}&difficulty=${encodeURIComponent(state.difficulty)}`);
    if (!d.total) { $('dailydist').textContent = 'No posted scores yet — be the first.'; return; }
    let atPar = 0;
    for (const [clicks, n] of Object.entries(d.hist))
      if (+clicks === state.par) atPar += n;
    $('dailydist').textContent =
      `${d.total} retained best score${d.total===1?'':'s'} (up to 100) · ${Math.round(100*atPar/d.total)}% of these best scores made par`;
  } catch { /* a missing stat line is not an error worth showing */ }
}

$('share').onclick = () => copyText(state.share, $('share'), 'Copy result');

// The pre-finish share: recruit an opponent, not an audience. The result
// share needs a result; the challenge only needs a race.
$('chal').onclick = () => {
  if (!state.goal) return;
  const from = state.trail[0] ? state.trail[0].title : '';
  copyText(
    `WikiGolf challenge ⛳ ${from} → ${state.goal.title} — par ${state.par}. Beat it:\n${state.url || location.href}`,
    $('chal'), 'Share challenge');
};

// Replay the optimal route on the map: frame it, then let draw() animate.
$('replay').onclick = async () => {
  closeOverlay('result');
  await openMap();
  $('mapview').value = 'points'; updateMapView();
  const best = (state.best || []).filter(a => a.x != null);
  if (best.length < 2) return;
  fitTo(best);
  state.replay = { t0: performance.now() + 350 }; requestMap(); // let the camera settle first
};

/* ---------------- pick your own race ---------------- */
// Two independent pickers. Each keeps the chosen id on the input element so
// the id, not the typed text, is what gets sent — titles are not unique enough
// to resolve server-side, and re-searching on submit could pick a different
// article than the one shown.
const picked = { from:null, to:null };

function attachPicker(inputId, hitsId, slot) {
  let t = null;
  const input = $(inputId), hits = $(hitsId);
  input.addEventListener('input', e => {
    clearTimeout(t);
    picked[slot] = null;
    input.classList.remove('picked');
    $('cgo').disabled = !(picked.from && picked.to);
    const q = e.target.value.trim();
    if (!q) { hits.innerHTML=''; return; }
    t = setTimeout(async () => {
      let res;
      try { res = await api('/api/search?limit=6&q='+encodeURIComponent(q)); }
      catch { return; }
      hits.innerHTML='';
      for (const h of res) {
        const el = document.createElement('div');
        el.className='hit';
        el.innerHTML = `${esc(h.title)} <span class="tag">${h.in_degree.toLocaleString()}</span>`;
        el.onclick = () => {
          picked[slot] = h.id;
          input.value = h.title;
          input.classList.add('picked');
          hits.innerHTML='';
          $('cerr').textContent='';
          $('cgo').disabled = !(picked.from && picked.to);
        };
        hits.appendChild(el);
      }
    }, 220);
  });
}
attachPicker('cfrom','chitsA','from');
attachPicker('cto','chitsB','to');

$('cgo').onclick = async () => {
  if (!picked.from || !picked.to) return;
  if (picked.from === picked.to) {
    $('cerr').textContent = 'Pick two different articles.';
    return;
  }
  $('cerr').textContent = 'Finding the shortest route…';
  state.trail=[]; state.steps=[]; state.cur=null; state.done=false;
  try {
    await startRace('/api/puzzle', `from=${picked.from}&to=${picked.to}`);
    $('cerr').textContent='';
    $('custom').open = false;
  } catch (e) {
    // A pair with no route under the current hub level is the common case, and
    // the hub slider is the fix — say so rather than just reporting failure.
    $('cerr').textContent = e.message || 'No route found. Try a lower hub level.';
  }
};

// Filtering and sorting are local to the links already on screen — no request,
// no debounce needed. Repainting a few hundred rows is imperceptible.
$('lfilter').addEventListener('input', paintLinks);
$('lsort').addEventListener('change', paintLinks);
$('ldetails').addEventListener('change', paintLinks);
$('lgroup').addEventListener('change', e => { state.group = e.target.checked; paintLinks(); });

let timer=null;
$('search').addEventListener('input', e => {
  clearTimeout(timer);
  const q = e.target.value.trim();
  if (!q) { $('hits').innerHTML=''; return; }
  timer = setTimeout(async () => {            // debounced: server-side full scan
    let hits;
    try {
      hits = await api('/api/search?limit=8&q='+encodeURIComponent(q));
    } catch (err) {
      // Was silent. /api/search is in the heavy rate-limit bucket, so typing
      // in more than one box at once can get a 429, and an unhandled rejection
      // here just left the list empty with no explanation.
      $('hits').innerHTML = `<div class="empty">${esc(err.message || 'search failed')}</div>`;
      return;
    }
    $('hits').innerHTML='';
    if (!hits.length) {
      $('hits').innerHTML = `<div class="empty">No article matches that.</div>`;
      return;
    }
    for (const h of hits) {
      const el=document.createElement('div'); el.className='hit';
      el.style.cssText='padding:6px 9px;cursor:pointer;border-radius:6px';
      el.onmouseenter=()=>el.style.background='var(--hover)';
      el.onmouseleave=()=>el.style.background='';
      el.innerHTML=`${esc(h.title)} <span class="tag">${h.in_degree.toLocaleString()}</span>`;
      el.onclick=()=>{
        $('hits').innerHTML=''; $('search').value='';
        if (!state.goal) return;
        // Jumping restarts the run: clear trail and step record together, or
        // the share squares would describe hops no longer on the board. It
        // also voids the score — you are no longer playing from the start, so
        // "beating par" from here would be a lie (the live server rejects the
        // leaderboard post for the same reason; the local card must agree).
        state.trail=[]; state.steps=[]; state.cur=null; state.done=false;
        state.jumped=true;
        closeOverlay('result');
        go(h.id).then(() => { setVeil(null); startClock(); });
      };
      $('hits').appendChild(el);
    }
  }, 220);
});

window.addEventListener('resize', () => {resize(); requestMap();});
window.addEventListener('keydown', e => {
  if (e.key === 'Escape' && !$('reader').hidden) { closeReader(); return; }
  if (e.target.tagName === 'INPUT') return;
  if (e.key === 'Backspace') { e.preventDefault(); $('back').click(); }
});
