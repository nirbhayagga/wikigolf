/* ---------------- map ---------------- */
const hue = c => ((c * 2654435761) >>> 0) % 360;
let REGIONS = {};
const regionName = c => (c == null ? 'Unplaced'
  : REGIONS[c] || REGIONS[String(c)] || `Region ${c}`);

let TH = {};
function readTheme() {
  const cs = getComputedStyle(document.documentElement);
  const v = n => cs.getPropertyValue(n).trim();
  TH = {
    bg: v('--canvas-bg'), ink: v('--canvas-ink'), halo: v('--canvas-halo'),
    s: v('--dot-s'), l: v('--dot-l'), a: parseFloat(v('--dot-a')) || .3,
    from: v('--from'), to: v('--to'), trail: v('--trail'),
    best: v('--route') || v('--accent'),
    cur: v('--text'), locked: v('--locked'),
  };
}
// Snapshotted rather than read per draw: getComputedStyle inside the point
// loop would force a style recalculation 45,000 times a frame.
const colorOf = c => c == null ? TH.locked : `hsl(${hue(c)},${TH.s},${TH.l})`;

const px = x => (x - cam.x) * cam.s + cv.width / 2;
// Canvas y grows downward and the layout's does not, so flip.
const py = y => cv.height / 2 - (y - cam.y) * cam.s;

function fitTo(pts, pad = 1.35) {
  if (!pts.length) return;
  let x0=Infinity,y0=Infinity,x1=-Infinity,y1=-Infinity;
  for (const p of pts) { x0=Math.min(x0,p.x); y0=Math.min(y0,p.y); x1=Math.max(x1,p.x); y1=Math.max(y1,p.y); }
  const w = Math.max(x1-x0, 40), h = Math.max(y1-y0, 40);
  target = { x:(x0+x1)/2, y:(y0+y1)/2,
             s: Math.min(cv.width/(w*pad), cv.height/(h*pad)) };
  requestMap();
}
function fitAll() {
  // Frame the points actually drawn, not the full dataset's off-screen tail.
  // The region filter also applies to Fit; it must not zoom back to the world.
  const region = $('mapregion').value;
  const points = [];
  if (MAP) for (let i=0; i<MAP.x.length; i++) {
    if (region && String(MAP.c[i]) !== region) continue;
    if (Number.isFinite(MAP.x[i]) && Number.isFinite(MAP.y[i]))
      points.push({x:MAP.x[i], y:MAP.y[i]});
  }
  if (points.length) fitTo(points, 1.12);
  else if (META?.bounds) {
    const [x0,y0,x1,y1] = META.bounds;
    fitTo([{x:x0,y:y0},{x:x1,y:y1}], 1.08);
  }
}

function resize() {
  const r = cv.getBoundingClientRect(), d = Math.min(window.devicePixelRatio||1, 2);
  cv.width = Math.max(1, r.width*d); cv.height = Math.max(1, r.height*d);
  if (!cam.s || cam.s === 1) { fitAll(); cam = {...target}; }
}

function frame(t) {
  if (!document.body.classList.contains('map-open') || $('mapview').value !== 'points') { RAF=null; return; }
  if (matchMedia('(prefers-reduced-motion: reduce)').matches) { cam={...target}; state.replay=null; }
  // Ease toward the target view.
  cam.x += (target.x-cam.x)*.12; cam.y += (target.y-cam.y)*.12;
  cam.s += (target.s-cam.s)*.12;
  draw(t);
  RAF=null;
  const moving=Math.abs(target.x-cam.x)+Math.abs(target.y-cam.y)+Math.abs(target.s-cam.s)>0.01;
  if ((moving || state.replay) && document.body.classList.contains('map-open')) RAF=requestAnimationFrame(frame);
}

function draw(t) {
  ctx.fillStyle = TH.bg; ctx.fillRect(0,0,cv.width,cv.height);
  if (!META) return;

  if (MAP) {
    // Additive blending makes dense regions glow rather than flatten out,
    // which is what gives the map visible structure.
    ctx.globalCompositeOperation = 'source-over';
    const r = Math.max(.7, Math.min(1.6, cam.s));
    const selectedRegion=$('mapregion').value;
    for (let i=0;i<MAP.x.length;i++) {
      if (selectedRegion && String(MAP.c[i]) !== selectedRegion) continue;
      const X = px(MAP.x[i]), Y = py(MAP.y[i]);
      if (X<-20||Y<-20||X>cv.width+20||Y>cv.height+20) continue;
      ctx.fillStyle = colorOf(MAP.c[i]); ctx.globalAlpha = TH.a;
      ctx.beginPath(); ctx.arc(X,Y,r,0,7); ctx.fill();
    }
    ctx.globalCompositeOperation = 'source-over'; ctx.globalAlpha = 1;
  }

  if (LAND) {
    ctx.textAlign='center'; ctx.textBaseline='middle';
    ctx.font = `600 ${Math.round(11*Math.min(2,Math.max(1,cam.s*.9)))}px ui-sans-serif, system-ui`;
    const occupied=[];
    for (const l of LAND) {
      if ($('mapregion').value && String(l.c) !== $('mapregion').value) continue;
      const X=px(l.x), Y=py(l.y);
      if (X<0||Y<0||X>cv.width||Y>cv.height) continue;
      const width=ctx.measureText(l.title).width+18, height=25;
      if (occupied.some(b=>Math.abs(b.x-X)<(b.w+width)/2 && Math.abs(b.y-Y)<height)) continue;
      occupied.push({x:X,y:Y,w:width});
      ctx.lineWidth=3.5; ctx.strokeStyle=`rgba(${TH.halo},.92)`;
      ctx.strokeText(l.title, X, Y);
      ctx.fillStyle=`rgba(${TH.ink},.72)`; ctx.fillText(l.title, X, Y);
    }
  }

  // The optimal route, drawn under your own so yours stays on top. Dashed and
  // in the accent colour: the point of showing it is the *comparison* — where
  // you diverged from it and how far you wandered — which only reads if both
  // are visible at once.
  const best = (state.best || []).filter(a => a.x != null);
  if (best.length > 1) {
    // Replay: reveal the route hop by hop with a dot travelling along it.
    // Progress is a pure function of the clock, so the continuous draw loop
    // animates it with no timers to manage.
    let upto = best.length - 1, frac = 0, dot = null;
    if (state.replay) {
      const t = Math.max(0, (performance.now() - state.replay.t0) / 700); // hops
      if (t >= best.length - 1) state.replay = null;
      else {
        upto = Math.floor(t); frac = t - upto;
        const a = best[upto], b = best[upto + 1];
        dot = { x: a.x + (b.x - a.x) * frac, y: a.y + (b.y - a.y) * frac };
      }
    }
    ctx.save();
    ctx.strokeStyle = TH.best; ctx.globalAlpha = .85; ctx.lineWidth = 1.8;
    ctx.setLineDash([6, 5]); ctx.lineJoin = 'round'; ctx.lineCap = 'round';
    ctx.beginPath(); ctx.moveTo(px(best[0].x), py(best[0].y));
    for (const p of best.slice(1, upto + 1)) ctx.lineTo(px(p.x), py(p.y));
    if (dot) ctx.lineTo(px(dot.x), py(dot.y));
    ctx.stroke(); ctx.restore();
    ctx.globalAlpha = 1;
    for (const p of best.slice(1, Math.max(1, upto + (dot ? 1 : 0)))) mark(p, TH.best, 2.5);
    if (dot) mark(dot, TH.best, 4);
  }

  const pts = state.trail.filter(a => a.x != null);
  if (pts.length > 1) {
    ctx.save();
    ctx.shadowColor=TH.trail; ctx.shadowBlur=10;
    ctx.strokeStyle=TH.trail; ctx.globalAlpha=.95; ctx.lineWidth=2.2;
    ctx.lineJoin='round'; ctx.lineCap='round';
    ctx.beginPath(); ctx.moveTo(px(pts[0].x),py(pts[0].y));
    for (const p of pts.slice(1)) ctx.lineTo(px(p.x),py(p.y));
    ctx.stroke(); ctx.restore();
    ctx.globalAlpha=1;
    for (const p of pts.slice(1,-1)) mark(p,TH.trail,3);
  }
  if (pts.length) mark(pts[0],TH.from,6,'From');
  if (state.goal && state.goal.x!=null) {
    const pulse = 8 + Math.sin((t||0)/380)*3;
    mark(state.goal,TH.to,6,'Goal',pulse);
  }
  if (state.cur && state.cur.x!=null) mark(state.cur,TH.cur,5);
}

function mark(p, color, r, label, ring) {
  const X=px(p.x), Y=py(p.y);
  ctx.save();
  if (ring) { ctx.beginPath(); ctx.arc(X,Y,ring,0,7);
    ctx.strokeStyle=color; ctx.globalAlpha=.55; ctx.lineWidth=1.6; ctx.stroke(); ctx.globalAlpha=1; }
  ctx.shadowColor=color; ctx.shadowBlur=12;
  ctx.beginPath(); ctx.arc(X,Y,r,0,7); ctx.fillStyle=color; ctx.fill();
  ctx.restore();
  if (label) {
    ctx.font='700 10px ui-sans-serif, system-ui'; ctx.textAlign='center';
    ctx.lineWidth=3; ctx.strokeStyle=`rgba(${TH.halo},.9)`;
    ctx.strokeText(label.toUpperCase(), X, Y-r-8);
    ctx.fillStyle=color; ctx.fillText(label.toUpperCase(), X, Y-r-8);
  }
}
