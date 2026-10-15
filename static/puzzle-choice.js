/* Random practice variety. Shared challenge and daily selection bypass this. */
const PuzzleChoice = (() => {
  const ids = row => Array.isArray(row) ? [row[0], row[2]] : [row.start.id, row.goal.id];
  const key = row => ids(row).join('-');
  function choose(rows, recentPairs = [], recentEndpoints = [], random = Math.random) {
    const unique = [...new Map(rows.map(row => [key(row), row])).values()];
    if (!unique.length) throw new Error('No puzzles match these settings.');
    const seen = new Set(recentPairs);
    const fresh = unique.filter(row => !seen.has(key(row)));
    const candidates = fresh.length ? fresh : unique;
    const counts = new Map();
    for (const id of recentEndpoints) counts.set(id, (counts.get(id) || 0) + 1);
    const score = row => ids(row).reduce((sum, id) => sum + (counts.get(id) || 0), 0);
    const best = Math.min(...candidates.map(score));
    const varied = candidates.filter(row => score(row) === best);
    return varied[Math.floor(random() * varied.length)];
  }
  return {choose, ids, key};
})();
if (typeof module !== 'undefined') module.exports = PuzzleChoice;
