/* Page composition, dialogs, optional exploration and device-local progress. */
function announce(message) { $('notice').textContent = message; $('notice').hidden = !message; }
function todayNumber() { return Math.floor(Date.now()/86400000) - META.day0 + 1; }
function setMode(mode) {
  document.body.dataset.mode = mode;
  for (const [id,m] of [['daily','daily'],['new','random'],['round','round']]) {
    $(id).classList.toggle('primary', mode === m);
    $(id).setAttribute('aria-pressed', String(mode === m));
  }
  $('hubctl').style.display = !STATIC && mode === 'random' ? 'block' : 'none';
  $('rpick').style.display = STATIC && mode === 'random' ? 'flex' : 'none';
  $('roundpick').style.display = !STATIC && mode === 'round' ? 'flex' : 'none';
  $('settings').open = false;
  announce('');
}
function renderJourney() {
  $('journeycount').textContent = `· ${Math.max(0,state.trail.length-1)} steps`;
  $('journeylist').innerHTML = state.trail.map((a,i) => `<li data-step="${i === 0 ? 'START' : 'STEP '+i}">${esc(a.title)}</li>`).join('');
}
$('articleinfo').ontoggle = () => {
  $('curmeta').hidden = !$('articleinfo').open;
  $('curalias').hidden = !$('articleinfo').open;
};
let ACTIVE_OVERLAY = null, RETURN_FOCUS = null;
function openOverlay(id) {
  if (ACTIVE_OVERLAY && ACTIVE_OVERLAY !== id) closeOverlay(ACTIVE_OVERLAY);
  RETURN_FOCUS = document.activeElement;
  ACTIVE_OVERLAY = id;
  $('gamepanel').inert = true;
  if (id === 'stage') document.body.classList.add('map-open');
  else if (id === 'reader') $('reader').hidden = false;
  else $(id).style.display = 'grid';
  const first = [...$(id).querySelectorAll('button,input,select,summary,a[href]')].find(el => !el.disabled && el.getClientRects().length);
  first?.focus();
}
function closeOverlay(id) {
  if (id === 'stage') document.body.classList.remove('map-open');
  else if (id === 'reader') $('reader').hidden = true;
  else $(id).style.display = 'none';
  if (ACTIVE_OVERLAY === id) {
    ACTIVE_OVERLAY = null;
    $('gamepanel').inert = false;
    if (RETURN_FOCUS?.isConnected) RETURN_FOCUS.focus();
  }
}
$('resultclose').onclick = () => closeOverlay('result');
window.addEventListener('keydown', e => {
  if (!ACTIVE_OVERLAY) return;
  if (e.key === 'Escape') { closeOverlay(ACTIVE_OVERLAY); return; }
  if (e.key !== 'Tab') return;
  const items = [...$(ACTIVE_OVERLAY).querySelectorAll('button,input,select,summary,a[href],[tabindex="0"]')]
    .filter(el => !el.disabled && el.getClientRects().length);
  const first=items[0], last=items[items.length-1];
  if (!first) { e.preventDefault(); return; }
  if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last.focus(); }
  else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
});

let MAP_LOADING = null, MAP_DIRTY = true, RAF = null;
function requestMap() {
  MAP_DIRTY = true;
  if (document.body.classList.contains('map-open') && RAF == null) RAF = requestAnimationFrame(frame);
}
async function loadMap() {
  if (!META?.has_map) throw new Error('The map is unavailable for this dataset.');
  if (!MAP_LOADING) MAP_LOADING = Promise.all([api('/api/map?n=45000'),api('/api/landmarks?n=45')])
    .then(([map,land]) => {
      MAP=map; LAND=land;
      const sizes = new Map();
      for (const c of MAP.c) sizes.set(c,(sizes.get(c)||0)+1);
      const seen = new Set();
      $('topiclist').replaceChildren();
      $('mapregion').innerHTML = '<option value="">All topics</option>';
      for (const l of LAND) {
        if (l.c == null || seen.has(l.c)) continue;
        seen.add(l.c);
        const title = regionName(l.c);
        const option = document.createElement('option'); option.value=l.c; option.textContent=l.title;
        $('mapregion').appendChild(option);
        const topic = document.createElement('option'); topic.value=l.c; topic.textContent=title;
        $('topic').appendChild(topic);
        const card = document.createElement('button'); card.className='topic-card';
        card.innerHTML=`<b>${esc(l.title)}</b><span>${(sizes.get(l.c)||0).toLocaleString()} articles in this sampled community<br>Automatic label: ${esc(title)}</span>`;
        card.onclick=()=> { $('mapview').value='points'; $('mapregion').value=String(l.c); updateMapView(); };
        $('topiclist').appendChild(card);
      }
    }).catch(e => { MAP_LOADING=null; throw e; });
  return MAP_LOADING;
}
async function openMap() {
  openOverlay('stage');
  await refreshMap();
}
async function refreshMap() {
  $('mapretry').disabled=true;
  $('mapstatus').hidden=true;
  $('mapcaption').textContent='Loading topics…';
  try { await loadMap(); }
  catch(e) { $('maperror').textContent=e.message; $('mapstatus').hidden=false; }
  finally { $('mapretry').disabled=false; }
  updateMapView();
}
function updateMapView() {
  const view=$('mapview').value;
  $('topiclist').hidden=view !== 'atlas';
  $('neighborlist').hidden=view !== 'neighbors';
  $('cv').hidden=view !== 'points';
  $('zoomer').hidden=view !== 'points';
  $('legend').hidden=view !== 'points';
  $('regionlabel').hidden=view !== 'points';
  if (view === 'neighbors') {
    $('mapcaption').textContent='A local view of actual outgoing links, ordered by how often they are linked. This view does not move your player.';
    const box=$('neighborlist');
    if (!state.cur || state.veil) { box.textContent='Start or resume your race to inspect its article links.'; return; }
    box.innerHTML=`<h2>${esc(state.cur.title)}</h2><p>${esc(state.cur.desc || '')}</p><p class="lbl2">${state.cur.links.length} outgoing links. Showing up to 24; all choices remain in the game.</p>`;
    const links=state.cur.links.filter(l=>!l.banned).slice().sort((a,b)=>b.in_degree-a.in_degree).slice(0,24);
    for(const l of links) {
      const row=document.createElement('div'); row.className='neighbor';
      row.innerHTML=`<span aria-hidden="true">↳</span><div><b>${esc(l.title)}</b><br><span>${esc(l.desc || '')}</span></div>`;
      box.appendChild(row);
    }
  } else if (view === 'atlas') {
    $('mapcaption').textContent=MAP ? 'Communities grouped by links, shown by a prominent article. Automatic labels may describe only part of a community.' : 'Topic data is unavailable. You can still inspect your current article’s links.';
  } else {
    $('mapcaption').textContent='A sample of well-linked articles. Screen distance does not measure the clicks to your goal.';
    resize();
    fitAll();
    requestMap();
  }
}
$('explore').onclick=openMap;
$('mapretry').onclick=refreshMap;
$('closemap').onclick=()=>closeOverlay('stage');
$('mapview').onchange=updateMapView;
$('mapregion').onchange=updateMapView;

let SAVED_PROGRESS = null;
try { SAVED_PROGRESS=JSON.parse(localStorage.getItem('wr-progress')); } catch {}
function saveProgress() {
  if (!META || !state.puzzle || !state.started || state.done || RESTORING) return;
  const elapsed=(state.veil==='paused' ? state.pauseAt : performance.now())-state.started;
  try { localStorage.setItem('wr-progress', JSON.stringify({
    dataset:RaceLink.dataset(META), url:state.url, ids:state.trail.map(a=>a.id),
    elapsed, compass:state.compass, dist:state.dist, depth:state.distDepth, exact:state.distExact,
    round:ROUND ? {idx:ROUND.idx, strokes:ROUND.strokes, pars:ROUND.pars} : null,
    saved:Date.now()
  })); } catch {}
}
function offerResume() {
  if (SAVED_PROGRESS && SAVED_PROGRESS.dataset===RaceLink.dataset(META) && Array.isArray(SAVED_PROGRESS.ids)
      && SAVED_PROGRESS.ids.length>0 && SAVED_PROGRESS.ids.length<=500 && Date.now()-SAVED_PROGRESS.saved<7*86400000)
    $('resumebar').hidden=false;
}
let RESTORING=false;
async function loadRaceLink(url) {
  const shared=RaceLink.decode(url,META);
  if(shared.mode==='pair') {
    const q=new URLSearchParams({from_title:shared.from,to_title:shared.to});
    if(shared.difficulty) q.set('difficulty',shared.difficulty);
    if(shared.ban!=null) { if(STATIC || shared.ban!=='none') q.set('ban_degree',shared.ban); else q.set('ban_top','0'); }
    await startRace('/api/puzzle',q.toString());
  } else if(shared.mode==='round') {
    if(STATIC && shared.holes!==3) throw new Error('This export supports three-hole rounds.');
    $('dnum').value=shared.number; $('rholes').value=shared.holes; await startRound();
  } else {
    $('ddiff').value=shared.difficulty; $('dnum').value=shared.number || ''; await startRace('/api/daily');
  }
}
$('resumeSaved').onclick=async()=> {
  if(!SAVED_PROGRESS || RESTORING) return;
  RESTORING=true;
  try {
    const saved=SAVED_PROGRESS;
    await loadRaceLink(saved.url);
    if(saved.round && ROUND) {
      if(!Number.isInteger(saved.round.idx) || saved.round.idx<0 || saved.round.idx>=ROUND.holes.length) throw new Error('Invalid saved round.');
      Object.assign(ROUND,saved.round);
      await adoptPuzzle(ROUND.holes[ROUND.idx],{archive:ROUND.archive,difficulty:'round'});
    }
    if(saved.ids[0]!==state.startId) throw new Error('Saved progress belongs to a different puzzle.');
    const trail=[state.cur];
    for(const id of saved.ids.slice(1)) {
      const previous=trail[trail.length-1];
      if(!previous.links.some(l=>l.id===id && !l.banned)) throw new Error('The saved route is no longer playable.');
      trail.push(await api(`/api/article/${id}?goal=${state.goal.id}${state.ban!=null?'&ban_degree='+state.ban:''}`));
    }
    state.trail=trail; state.cur=trail[trail.length-1];
    state.steps=trail.slice(1).map((_,i)=>i<state.par);
    state.compass=Math.min(state.compass,Math.max(0,Number(saved.compass)||0));
    if(STATIC) S_RUNS.get(+state.run).charges=state.compass;
    state.practice=!STATIC;
    await go(state.cur.id,true,true);
    state.dist=saved.dist; state.distDepth=saved.depth; state.distExact=saved.exact;
    paintLinks();
    state.pauseAt=performance.now(); state.started=state.pauseAt-Math.max(0,Number(saved.elapsed)||0);
    $('time').textContent=mmss(state.pauseAt-state.started);
    setVeil('paused'); updateCompass(); renderJourney();
    announce(STATIC ? 'Progress restored. Resume when you’re ready.' : 'Progress restored as practice. Server-ranked races must finish in one session.');
    $('resumebar').hidden=true;
  } catch(e) { announce(e.message); }
  finally { RESTORING=false; }
};
$('discardSaved').onclick=()=> { SAVED_PROGRESS=null; $('resumebar').hidden=true; try {localStorage.removeItem('wr-progress');}catch{} };
window.addEventListener('pagehide',saveProgress);
