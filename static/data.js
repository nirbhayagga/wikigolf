/* ---------------- static mode ----------------
   The exporter stamps data-static on <html>, and every api() call routes
   into a file-tree adapter instead of /api. What the files cannot answer
   (custom-pair par, the compass, live boards, "a shortest route") the
   static init hides, so nothing on screen ever promises what a file host
   cannot deliver. (STATIC itself is declared up by blank(), which needs it.) */
const S_CACHE = new Map();     // path -> parsed json, shards LRU-capped
let S_RUNS = new Map();        // fake run id -> {par, routes}
let S_NEXTRUN = 1;

async function sj(path) {
  if (S_CACHE.has(path)) return S_CACHE.get(path);
  const r = await fetch(path);
  if (!r.ok) throw new Error(`missing ${path}`);
  const j = await r.json();
  if (S_CACHE.size > 60) S_CACHE.delete(S_CACHE.keys().next().value);
  S_CACHE.set(path, j);
  return j;
}

function sIssue(p, number, ckey, charges) {
  const run = S_NEXTRUN++;
  S_RUNS.set(run, { routes: p.routes ?? 0, par: p.par, number, ckey, charges,
                    goal: p.goal ? p.goal.id : null, ban: p.ban_degree ?? null });
  return run;
}

function sPuzzle(raw, difficulty, number, meta, goalCompass) {
  // Compass-lite: dailies and rounds inside the exported window carry a
  // per-day compass file, and pool races a per-goal one (goalCompass, from
  // the pool file's own flag). Charges mirror the live formula:
  // clamp(par - 2, 1, 4).
  const ckey = difficulty.startsWith('round-h')
    ? 'h' + difficulty.slice('round-h'.length) : difficulty;
  // cd is the goal's compass depth, baked at export. Below 2 the compass
  // can only say ">1" on every link, so it is not offered at all.
  const avail = meta && (meta.compass_days ?? 0) > 0
    && (raw.cd == null || raw.cd >= 2)
    && (goalCompass
    || (number != null
        && number >= 1 && number <= meta.today_number + meta.compass_days));
  const charges = avail ? Math.min(Math.max(raw.par - 2, 1), 4) : 0;
  return { start: raw.start, goal: raw.goal, optimal: raw.par,
    ban_degree: raw.ban_degree ?? null, difficulty, number: number ?? null,
    route: raw.route ?? null,   // baked shortest route, shown post-race
    compass: charges, run: sIssue(raw, number, ckey, charges) };
}

async function staticApi(p, o) {
  const [path, qs] = p.split('?');
  const q = new URLSearchParams(qs || '');
  const meta = await sj('meta.json');

  if (path === '/api/meta') return meta;
  if (path === '/api/regions') return sj('regions.json');
  if (path === '/api/landmarks') return sj('landmarks.json');
  if (path === '/api/map') return sj('map.json');
  if (path === '/api/hubs') return { ban_degree: null, excluded: 0, sample: [] };
  if (path === '/api/dist') return { total: 0, hist: {} };

  if (path.startsWith('/api/article/')) {
    const id = +path.slice('/api/article/'.length);
    const shard = await sj(`shards/${Math.floor(id / meta.shard_size)}.json`);
    // v2 shards are {a: entries, d: target dict}; v1 were a bare array.
    const arr = Array.isArray(shard) ? shard : shard.a;
    const dict = Array.isArray(shard) ? {} : (shard.d || {});
    const e = arr.find(a => a[0] === id);
    if (!e) throw new Error('no such article');
    const ban = q.get('ban_degree') ? +q.get('ban_degree') : null;
    const goal = q.get('goal') ? +q.get('goal') : null;
    const [eid, title, desc, kind, flags, views, community, x, y, deg, links] = e;
    return { id: eid, title, desc, kind, views: views || undefined,
      featured: !!(flags & 6), disambig: !!(flags & 1),
      community, x, y, in_degree: deg,
      links: links.map(([lid, lt, ld]) => {
        const t = dict[lid];
        return { id: lid, title: lt, in_degree: ld,
          desc: t ? (t[0] ?? undefined) : undefined,
          disambig: !!((t?.[1] || 0) & 1), featured: !!((t?.[1] || 0) & 6),
          banned: ban != null && lid !== goal && ld > ban };
      }) };
  }

  if (path === '/api/compass') {
    const body = JSON.parse(o.body);
    const r = S_RUNS.get(+body.run);
    if (!r) throw new Error('unknown or expired run');
    if (!(r.charges > 0)) throw new Error('no compass charges left');
    // Dailies/rounds share a per-day file keyed by difficulty; random races
    // (number == null) get a per-goal file instead.
    const cf = await sj(r.number != null
        ? `compass/${r.number}.json` : `compass/g${r.goal}.json`)
      .catch(() => { throw new Error('no compass data for this race'); });
    const entry = r.number != null ? cf[r.ckey] : cf;
    if (!entry) throw new Error('no compass data for this race');
    // Levels are delta-encoded sorted ids; level i+1 = that many clicks out.
    const dmap = new Map();
    entry.l.forEach((lvl, i) => {
      let id = 0;
      for (const gap of lvl) { id += gap; dmap.set(id, i + 1); }
    });
    const shard = await sj(`shards/${Math.floor(body.from / meta.shard_size)}.json`);
    const arr = Array.isArray(shard) ? shard : shard.a;
    const e = arr.find(a => a[0] === body.from);
    if (!e) throw new Error('no such article');
    const links = e[10].map(([lid, , degree]) => ({ id: lid,
      dist: lid === r.goal ? 0 : (dmap.get(lid) ?? null),
      banned: r.ban != null && degree > r.ban && lid !== r.goal }));
    const useful = links.some(l => !l.banned && l.dist != null);
    if (useful) {
      // Recheck after the asynchronous loads so concurrent calls cannot overspend.
      if (!(r.charges > 0)) throw new Error('no compass charges left');
      r.charges--;
    }
    return { charges_left: r.charges, depth: entry.d, exact: false,
      charged: useful, links };
  }

  if (path === '/api/daily' || path === '/api/course') {
    // Today comes from the CLIENT clock, not meta.json — the exported
    // today_number is frozen at export time, and using it served the same
    // daily forever. day0 is the shared epoch both sides derive from.
    const todayNum = Math.floor(Date.now() / 86400000) - meta.day0 + 1;
    const number = +(q.get('number') || todayNum);
    if (!Number.isSafeInteger(number) || number > todayNum) throw new Error('That daily does not exist yet.');
    if (number < 1) throw new Error('that daily does not exist');
    if (number > meta.today_number + meta.days_ahead)
      throw new Error(number <= todayNum
        ? 'this build has gone stale — it needs a re-export and redeploy'
        : 'that daily does not exist yet');
    const d = await sj(`daily/${number}.json`);
    if (path === '/api/daily') {
      const dif = q.get('difficulty') || 'medium';
      const raw = d.difficulties[dif];
      if (!raw) throw new Error('could not load the daily');
      return sPuzzle(raw, dif, d.number, meta);
    }
    return { number: d.number, par: d.round.par,
      holes: d.round.holes.map((h, i) => sPuzzle(h, `round-h${i+1}`, d.number, meta)) };
  }

  if (path === '/api/puzzle') {
    if (q.get('from_title') || q.get('to_title')) {
      const preferred = q.get('difficulty');
      const diffs = ['easy','medium','hard'].includes(preferred) ? [preferred] : ['easy','medium','hard'];
      for (const dif of diffs) {
        const rp = await sj(`random/${dif}.json`);
        const raw = rp.races.find(r => !Array.isArray(r) && r.start.title === q.get('from_title') && r.goal.title === q.get('to_title'));
        const ban = q.get('ban_degree');
        if (raw && (ban == null || (ban === 'none' ? rp.ban_degree == null : Number(ban) === rp.ban_degree)))
          return sPuzzle({...raw, ban_degree:rp.ban_degree}, dif, null, meta, !!rp.compass);
      }
      throw new Error('This shared race is not in this export. Choose a daily or a new race.');
    }
    if (q.get('from') || q.get('to') || q.get('region'))
      throw new Error('custom and topic races need the live server');
    const dif = q.get('difficulty') || 'easy';
    const rp = await sj(`random/${dif}.json`);
    // Par filter: pool rows carry the exact shortest distance, so choosing
    // par 5 really means par 5 — not a walk-length upper bound.
    const wantPar = +q.get('par') || 0;
    let pool = rp.races;
    if (wantPar) {
      pool = pool.filter(r => (Array.isArray(r) ? r[4] : r.par) === wantPar);
      if (!pool.length)
        throw new Error(`the ${dif} pool has no par-${wantPar} races — try another difficulty`);
    }
    // Keep pair history per difficulty and endpoint history across practice
    // difficulties. Dataset scoping prevents old article IDs affecting a new dump.
    const prefix = `wr-practice-${RaceLink.dataset(meta)}`;
    const pairKey = `${prefix}-${dif}`, endpointKey = `${prefix}-endpoints`;
    const readHistory = key => {
      try { const value = JSON.parse(localStorage.getItem(key)); return Array.isArray(value) ? value : []; }
      catch { return []; }
    };
    const recent = readHistory(pairKey).filter(x => typeof x === 'string').slice(-60);
    const endpoints = readHistory(endpointKey).filter(Number.isSafeInteger).slice(-12);
    const r = PuzzleChoice.choose(pool, recent, endpoints);
    recent.push(PuzzleChoice.key(r));
    endpoints.push(...PuzzleChoice.ids(r));
    try {
      localStorage.setItem(pairKey, JSON.stringify(recent.slice(-60)));
      localStorage.setItem(endpointKey, JSON.stringify(endpoints.slice(-12)));
    } catch {}
    // v2 pool rows are {start, goal, par, routes} with full article objects
    // (desc, in_degree, views, x/y — the goal card and red map dot need
    // them). v1 rows were bare tuples; a cached file can still serve one.
    const raw = Array.isArray(r)
      ? { start: { id: r[0], title: r[1] }, goal: { id: r[2], title: r[3] },
          par: r[4], routes: r[5] }
      : r;
    return sPuzzle({ ...raw, ban_degree: rp.ban_degree }, dif, null, meta,
                   !!rp.compass);
  }

  if (path === '/api/routes') {
    const r = S_RUNS.get(+q.get('run'));
    if (!r) throw new Error('unknown or expired run');
    // Mirror the live response: the card says "N different ${clicks}-click
    // routes", and pool counts saturate at u32::MAX ("at least this many").
    return { routes: r.routes, clicks: r.par,
             saturated: r.routes >= 4294967295, exact: true };
  }

  // No /api/search: the jump bar is hidden on static (mid-race teleport was
  // its only function, and playtesters mistook it for the link filter), so
  // the exporter stopped shipping the 2,417 search bucket files.
  throw new Error('not available in the static build');
}

async function api(p, o) {
  if (STATIC) return staticApi(p, o);
  const r = await fetch(p, o);
  if (!r.ok) throw new Error((await r.json().catch(()=>({}))).error || r.statusText);
  return r.json();
}

/* Runs live in server memory, so a restart mid-race forgets the id this tab
   is holding while the race itself is still perfectly playable. Re-issue the
   identical race — same endpoints, same exact ban via ban_degree — adopt the
   fresh run id, and retry the failed call once. The trail and clock are
   local, so the player never notices. Submits are deliberately NOT recovered:
   a re-issued run's clock starts now, and a leaderboard time measured from a
   restart would be a lie. */
async function reissueRun() {
  if (state.startId == null || !state.goal) throw new Error('no active race');
  let q = `from=${state.startId}&to=${state.goal.id}`;
  if (state.ban != null) q += `&ban_degree=${state.ban}`;
  const p = await api(`/api/puzzle?${q}`);
  state.run = p.run;
  return p;
}
async function apiRun(call) {
  try { return await call(); }
  catch (e) {
    if (!/unknown or expired/.test(e.message || '')) throw e;
    await reissueRun();
    return call();
  }
}
