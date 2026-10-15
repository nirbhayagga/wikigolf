const $ = id => document.getElementById(id);
const cv = $('cv'), ctx = cv.getContext('2d');
const esc = s => String(s).replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
const mmss = ms => { const s = Math.floor(ms/1000);
  return `${Math.floor(s/60)}:${String(s%60).padStart(2,'0')}`; };

let META=null, MAP=null, LAND=null, TICK=null;
// cam is where we are; target is where we want to be. Every frame eases one
// toward the other, so the view glides after each hop instead of snapping.
let cam={x:0,y:0,s:1}, target={x:0,y:0,s:1}, dragging=null;

// Declared before blank(): the state default below reads it.
const STATIC = document.documentElement.hasAttribute('data-static');

const blank = () => ({
  goal:null, cur:null, trail:[], steps:[], par:null, ban:null, run:null,
  done:false, started:null, elapsed:0, number:null, difficulty:'easy', share:'',
  veil:null, pauseAt:0, url:'',
  route:null,             // static: the baked shortest route, shown post-race
  jumped:false,           // a search jump moved the start: the run can't be scored
  compass:0, dist:null,   // charges left, and id -> clicks-to-goal for this article
  best:null,              // the optimal route, drawn on the map once the race ends
  // Shards carry no region per link, so grouping on static would put every
  // link under one "Unplaced" header. The checkbox is hidden there too —
  // both must agree, because blank() runs on every new race.
  group:!STATIC,
});
let state = blank();

const fmtViews = v => v >= 1e6 ? (v/1e6).toFixed(1)+'M'
  : v >= 1e3 ? Math.round(v/1e3)+'k' : String(v);

/* The streak was invisible except in the win verdict — a retention hook
   nobody could find. This chip sits by the daily controls whenever a
   streak exists. */
function updateStreakChip() {
  let st = {};
  try { st = JSON.parse(localStorage.getItem('wr-streak')) || {}; } catch {}
  const el = $('streakchip');
  if (!st.streak) { el.style.display = 'none'; return; }
  el.style.display = '';
  el.textContent = `${st.streak}-day streak`;
  el.title = `Daily streak: ${st.streak} (best ${st.best || st.streak}). ` +
    `Win consecutive dailies on this device to grow it.`;
}
