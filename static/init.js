(async function init(){
  let preferredTheme='midnight';
  try { preferredTheme=localStorage.getItem('wr-theme') || 'midnight'; } catch {}
  setTheme(preferredTheme);
  updateStreakChip();
  if (STATIC) {
    // The compass stays: compass-lite files cover dailies and rounds, and
    // updateCompass hides the button per-race when there is no data.
    // The jump search bar goes too: its only function is mid-race teleport
    // (unscored), the first playtester mistook it for the link filter, and
    // the exporter no longer ships search buckets to back it.
    for (const id of ['hubctl', 'custom', 'search', 'hits']) {
      const el = $(id); if (el) el.style.display = 'none';
    }
    // Boards need a server; the round stays 3 holes (9 isn't exported).
    $('nick').parentElement.style.display = 'none';
    $('board').parentElement.style.display = 'none';
    $('rholes').style.display = 'none';
    $('lgroup').checked = false; // shards carry no region per link
    $('lgroup').parentElement.style.display = 'none';
    // Shard links carry no read counts, so that sort would be a no-op lie.
    for (const opt of [...$('lsort').options]) if (opt.value === 'read') opt.remove();
  } else {
    // Live free play is steered by the hub slider; the pool picker is the
    // static replacement for it.
    $('rpick').style.display = 'none';
  }
  META = await api('/api/meta');
  try { REGIONS = await api('/api/regions'); } catch { REGIONS = {}; }
  if (!STATIC && META.duels) $('duelbox').style.display = '';
  $('mapmeta').textContent =
    `${META.articles.toLocaleString()} articles · ${META.edges.toLocaleString()} links`;
  readTheme();
  describeHubs();
  // First visit only, unless it was turned off. Shown after the race is armed
  // so the goal is already on screen behind it, and the clock is not running
  // either way — a race is armed, not started.
  // A share-link arrival lands in the armed race, not behind a popup — the
  // "?" button is right there for anyone who wants the rules first.

  // The inline instructions replace the first-visit modal.

  try {
    await loadRaceLink(location.href);
    offerResume();
  } catch(e) { announce(e.message); }

})().catch(e => announce(`Could not load the game: ${e.message}. Reload to retry.`));

// Static edition only: the live server has no sw.js, and caching /api/* would
// be wrong anyway. The worker's cache name rotates with each export, so a
// deploy invalidates everything at once.
if (STATIC && 'serviceWorker' in navigator) {
  navigator.serviceWorker.register('/sw.js').catch(()=>{});
}
