/* Pure URL contract, shared by the page and the regression suite. */
const RaceLink = (() => {
  const difficulties = new Set(['easy', 'medium', 'hard']);
  const dataset = meta => meta.dataset || `legacy-${meta.articles}-${meta.edges}-${meta.day0 ?? 0}`;
  function encode(base, p, meta, round) {
    const u = new URL(base); u.search = ''; u.hash = '';
    u.searchParams.set('dataset', dataset(meta));
    if (round) {
      u.searchParams.set('round', round.number);
      u.searchParams.set('holes', round.holes.length);
    } else if (p.number != null) {
      u.searchParams.set('daily', p.number);
      u.searchParams.set('difficulty', p.difficulty);
    } else {
      u.searchParams.set('from', p.start.title);
      u.searchParams.set('to', p.goal.title);
      u.searchParams.set('difficulty', p.difficulty || 'custom');
      u.searchParams.set('ban', p.ban_degree == null ? 'none' : p.ban_degree);
    }
    return u.toString();
  }
  function decode(url, meta) {
    const q = new URL(url).searchParams;
    if (q.has('dataset') && q.get('dataset') !== dataset(meta))
      throw new Error('This race belongs to a different Wikipedia dataset. Choose a new race.');
    const positive = key => {
      const v = Number(q.get(key));
      if (!Number.isSafeInteger(v) || v < 1) throw new Error(`Invalid ${key} number.`);
      return v;
    };
    const difficulty = q.get('difficulty') || 'medium';
    if (q.has('round')) {
      const holes = Number(q.get('holes') || 3);
      if (![3,9].includes(holes)) throw new Error('A round has 3 or 9 holes.');
      return {mode:'round', number:positive('round'), holes};
    }
    if (q.has('daily')) {
      if (!difficulties.has(difficulty)) throw new Error('Unknown difficulty.');
      return {mode:'daily', number:positive('daily'), difficulty};
    }
    if (q.has('from') || q.has('to')) {
      if (!q.get('from') || !q.get('to')) throw new Error('The race link needs both articles.');
      const ban = q.get('ban');
      if (ban != null && ban !== 'none' && (!/^\d+$/.test(ban) || !Number.isSafeInteger(Number(ban))))
        throw new Error('Invalid hub rule.');
      return {mode:'pair', from:q.get('from'), to:q.get('to'), difficulty:q.get('difficulty'), ban};
    }
    return {mode:'daily', difficulty:'medium'};
  }
  return {encode, decode, dataset};
})();
if (typeof module !== 'undefined') module.exports = RaceLink;
