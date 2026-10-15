#!/usr/bin/env python3
"""Audit an existing static puzzle export without changing it or loading Parquet.

Reports every random-pool row, then checks a deterministic 20-race review set
against exported article shards. Pageviews are a familiarity proxy, not a
judgment of cultural importance or an estimate of completion probability.
"""
import argparse
from collections import Counter, defaultdict
from html import escape
import json
from pathlib import Path
import random
import statistics
from urllib.parse import urlencode

DIFFICULTIES = ('easy', 'medium', 'hard')
SHOWCASE = (
    ('easy', 'Glasgow', 'Stephen Colbert'),
    ('easy', 'Eukaryote', 'The Weeknd'),
    ('medium', 'Esperanto', 'Kate Bush'),
    ('medium', 'IP address', 'McCarthyism'),
    ('hard', "Newton's laws of motion", 'Helena Bonham Carter'),
    ('hard', 'Winnie-the-Pooh', 'Ja Rule'),
)

# Editorial judgments for this snapshot, deliberately separate from metrics.
EDITORIAL = {
    ('Glasgow', 'Stephen Colbert'): (True, 'Good introductory candidate: familiar endpoints and a broad city-to-culture route. Verify that players can find the bridge without route knowledge.'),
    ('Eukaryote', 'The Weeknd'): (False, 'Hold back: the Vindhya Range and Konkani language bridge is hard to anticipate. Familiar endpoints alone do not make a fair introductory puzzle.'),
    ('Esperanto', 'Kate Bush'): (False, 'Hold back for specialist playtesting: the religion-to-music bridge is clever but difficult to infer. Better as a later themed challenge.'),
    ('IP address', 'McCarthyism'): (True, 'Strong thematic candidate: personal data and privacy give the route a comprehensible connection. Two optimal routes make it relatively unforgiving.'),
    ("Newton's laws of motion", 'Helena Bonham Carter'): (True, 'Candidate for an advanced showcase: a surprising science-to-film route with many alternatives. Needs playtesting before calling it fair.'),
    ('Winnie-the-Pooh', 'Ja Rule'): (False, 'Hold back: the baked route depends on biographical/date links rather than an obvious conceptual bridge. Many shortest routes do not establish enjoyment.'),
    ('Bill Gates', 'Surgeon'): (True, 'Strong general-audience candidate: bioterrorism and triage form an intelligible health-related bridge, with many optimal routes.'),
}


def familiarity(row):
    return min(row[k].get('views') or 0 for k in ('start', 'goal'))


def route_errors(row):
    route = row.get('route') or []
    errors = []
    if len(route) != row['par'] + 1:
        errors.append('Baked route length differs from par')
    if not route or route[0][0] != row['start']['id'] or route[-1][0] != row['goal']['id']:
        errors.append('Baked route endpoints differ from puzzle')
    if len({a[0] for a in route}) != len(route):
        errors.append('Baked route repeats an article')
    return errors


def summarize(rows):
    endpoints = [r[k] for r in rows for k in ('start', 'goal')]
    known_views = [a['views'] for a in endpoints if a.get('views') is not None]
    counts = Counter(a['id'] for a in endpoints)
    titles = {a['id']: a['title'] for a in endpoints}
    return dict(
        races=len(rows), unique_pairs=len({(r['start']['id'], r['goal']['id']) for r in rows}),
        pars=dict(sorted(Counter(r['par'] for r in rows).items())),
        missing_descriptions=sum(not a.get('desc') for a in endpoints),
        missing_views=len(endpoints)-len(known_views),
        low_view_endpoints=sum(v < 1000 for v in known_views),
        low_view_races=sum(any(a.get('views') is not None and a['views'] < 1000
                               for a in (r['start'], r['goal'])) for r in rows),
        median_views=statistics.median(known_views) if known_views else None,
        one_route_races=sum(r.get('routes') == 1 for r in rows),
        structural_failures=sum(bool(route_errors(r)) for r in rows),
        repeated=[dict(id=i, title=titles[i], appearances=n) for i, n in counts.most_common(8)],
    )


def choose_review(pools):
    selected = {}

    def add(d, row, reason):
        key = (d, row['start']['id'], row['goal']['id'])
        if key not in selected:
            selected[key] = dict(difficulty=d, reason=reason, puzzle=row, errors=route_errors(row))

    for d, start, goal in SHOWCASE:
        for row in pools[d]['races']:
            if row['start']['title'] == start and row['goal']['title'] == goal:
                add(d, row, 'Showcase candidate: editorial shortlist, not human-playtested')
                break
    for d in DIFFICULTIES:
        rows = pools[d]['races']
        for row in sorted(rows, key=familiarity)[:2]:
            add(d, row, 'Review: low pageview familiarity proxy')
        counts = Counter(a[k]['id'] for a in rows for k in ('start', 'goal'))
        if counts:
            repeated = counts.most_common(1)[0][0]
            row = next(r for r in rows if repeated in (r['start']['id'], r['goal']['id']))
            add(d, row, 'Review: most frequently repeated endpoint')
    remaining = [(d, row) for d in DIFFICULTIES for row in pools[d]['races']]
    random.Random(20261001).shuffle(remaining)
    for d, row in remaining:
        if len(selected) >= 20:
            break
        add(d, row, 'Seeded spot check (seed 20261001)')
    return list(selected.values())


def verify_shards(root, meta, review):
    # Load each required shard once, retain only the route articles' adjacency.
    wanted = defaultdict(set)
    for item in review:
        for a in item['puzzle'].get('route', []):
            wanted[a[0] // meta['shard_size']].add(a[0])
    articles = {}
    for shard, ids in sorted(wanted.items()):
        raw = json.loads((root / 'shards' / f'{shard}.json').read_text())
        for entry in raw if isinstance(raw, list) else raw['a']:
            if entry[0] in ids:
                articles[entry[0]] = dict(title=entry[1], flags=entry[4], degree=entry[9],
                                         links={link[0]: link[2] for link in entry[10]})
    for item in review:
        row, errors = item['puzzle'], item['errors']
        route = row.get('route') or []
        for i, node in enumerate(route):
            a = articles.get(node[0])
            if a is None:
                errors.append(f'Missing article {node[0]} in export')
                continue
            if node[1] != a['title']:
                errors.append(f'Title identity mismatch: {node[1]}')
            if i in (0, len(route)-1) and a['flags'] & 1:
                errors.append(f'Disambiguation endpoint: {a["title"]}')
            if 0 < i < len(route)-1 and item['ban_degree'] is not None and a['degree'] > item['ban_degree']:
                errors.append(f'Banned intermediate hub: {a["title"]}')
            if i+1 < len(route) and route[i+1][0] not in a['links']:
                errors.append(f'No exported link from {node[1]} to {route[i+1][1]}')
        item['route_verified'] = not errors
        start = articles.get(row['start']['id'])
        compass = root / 'compass' / f"g{row['goal']['id']}.json"
        item['opening_hint'] = None
        if start and compass.exists():
            data = json.loads(compass.read_text())
            legal = {i for i, degree in start['links'].items()
                     if item['ban_degree'] is None or degree <= item['ban_degree']
                     or i == row['goal']['id']}
            measured = {row['goal']['id']} & legal
            for level in data['l']:
                article_id = 0
                for delta in level:
                    article_id += delta
                    if article_id in legal:
                        measured.add(article_id)
            item['opening_hint'] = dict(depth=data['d'], legal_options=len(legal),
                numbered_options=len(measured), offered=data['d'] >= 2,
                unrestricted=item['ban_degree'] is not None)
    return len(wanted)


def build_report(root):
    meta = json.loads((root / 'meta.json').read_text())
    pools = {d: json.loads((root / 'random' / f'{d}.json').read_text()) for d in DIFFICULTIES}
    review = choose_review(pools)
    dataset = meta.get('dataset') or f"legacy-{meta['articles']}-{meta['edges']}-{meta['day0']}"
    for item in review:
        row, d = item['puzzle'], item['difficulty']
        item['showcase_recommended'], item['editorial_note'] = EDITORIAL.get(
            (row['start']['title'], row['goal']['title']),
            (False, 'Review endpoint familiarity, the clarity of link descriptions, and whether the bridge is discoverable without seeing the solution.'))
        item['ban_degree'] = pools[d]['ban_degree']
        item['challenge_query'] = '?' + urlencode(dict(dataset=dataset, difficulty=d,
            ban=item['ban_degree'] if item['ban_degree'] is not None else 'none',
            **{'from': row['start']['title'], 'to': row['goal']['title']}))
    shards = verify_shards(root, meta, review)
    return dict(export=str(root.resolve()), dataset=dataset, summaries={
        d: summarize(pools[d]['races']) for d in DIFFICULTIES}, review=review,
        checked_shards=shards,
        limitations='This is a data and editorial audit, not observed human playtesting. '
        'Sample routes are checked for real exported edges and legal hub rules; '
        'shortest-path optimality and route counts are not independently recomputed. '
        'Pageviews come from the export and may reflect transient news interest. '
        'The sample is deliberately mixed, not statistically representative.')


def render_html(report):
    esc = lambda value: escape(str(value), quote=True)
    rows = []
    for d, s in report['summaries'].items():
        rows.append('<tr>' + ''.join(f'<td>{esc(v)}</td>' for v in [d, s['races'],
            s['races']-s['unique_pairs'], s['low_view_races'], s['missing_descriptions'],
            s['median_views'], s['structural_failures']]) + '</tr>')
    cards = []
    for item in report['review']:
        p = item['puzzle']
        endpoints = ''.join(f'<p><b>{esc(p[k]["title"])}</b>: {esc(p[k].get("desc") or "No description")} '
            f'· {esc(p[k].get("views"))} exported monthly views</p>' for k in ('start', 'goal'))
        hint = item['opening_hint']
        hint_text = ('No per-goal compass file found' if hint is None else
            f"Opening hint: {hint['numbered_options']} of {hint['legal_options']} legal options have a numeric distance at depth {hint['depth']}. "
            + ('Distances ignore banned hubs and are lower bounds.' if hint['unrestricted'] else ''))
        route = ' → '.join(esc(a[1]) for a in p.get('route', []))
        status = 'Legal route verified against exported shards' if item['route_verified'] else '; '.join(item['errors'])
        cards.append(f'<article><small>{esc(item["reason"])}</small><h3>{esc(p["start"]["title"])} → '
            f'{esc(p["goal"]["title"])}</h3><p>{esc(item["difficulty"])} · par {p["par"]} · '
            f'{esc(p.get("routes"))} baked optimal routes</p>{endpoints}<p>{esc(status)}</p>'
            f'<p>{esc(hint_text)}</p>'
            f'<p><b>Editorial assessment:</b> {esc(item["editorial_note"])}</p>'
            f'<details><summary>Route and exact challenge query</summary><p>{route}</p>'
            f'<code>{esc(item["challenge_query"])}</code></details></article>')
    shortlist = ''.join(f'<li>{esc(i["puzzle"]["start"]["title"])} → {esc(i["puzzle"]["goal"]["title"])} ({esc(i["difficulty"])})</li>' for i in report['review'] if i['showcase_recommended'])
    hint_checks = [i['opening_hint'] for i in report['review'] if i['opening_hint'] and i['opening_hint']['offered']]
    blank_hints = sum(h['numbered_options'] == 0 for h in hint_checks)
    repeats = ''.join(f'<h3>{esc(d)}</h3><ul>' + ''.join(
        f'<li>{esc(r["title"])} — {r["appearances"]} endpoint appearances</li>' for r in s['repeated'])
        + '</ul>' for d, s in report['summaries'].items())
    return f'''<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>Puzzle pool audit</title><style>body{{font:16px/1.6 system-ui;background:#f5f5f0;color:#17222a;max-width:1050px;margin:40px auto;padding:0 24px}}h1,h2,h3{{line-height:1.2}}article{{background:white;border:1px solid #d4d9d6;border-radius:10px;padding:22px;margin:18px 0}}small{{color:#52655d}}table{{width:100%;border-collapse:collapse;font-size:14px}}td,th{{padding:10px;text-align:left;border-bottom:1px solid #ccd3ce}}code{{overflow-wrap:anywhere}}.table{{overflow:auto}}</style>
<h1>Puzzle pool audit</h1><p>Dataset: <code>{esc(report['dataset'])}</code></p>
<p>{esc(report['limitations'])}</p><h2>All random-pool rows</h2><div class="table"><table><tr>
<th>Difficulty</th><th>Races</th><th>Duplicate rows</th><th>Races with endpoint below 1,000 views</th>
<th>Missing endpoint descriptions</th><th>Median endpoint views</th><th>Malformed baked routes</th></tr>
{''.join(rows)}</table></div><p>Counts are occurrences, not unique articles. Low views are a review flag, not an automatic exclusion rule.</p>
<h2>Recommendations</h2><ol><li>Use identical route-count weighting in live and static random draws; the exporter previously ignored the easy/hard halves.</li>
<li>For the next full export, sample unique pairs and introduce a soft endpoint repetition cap with an explicit fallback for small pools.</li>
<li>Review low-view and missing-description endpoints before increasing difficulty. A longer path should not depend on knowing an obscure taxon.</li>
<li>Review hint timing: in {blank_hints} of {len(hint_checks)} inspected opening hints, no legal option gets a numeric distance. Explain this limitation and consider preserving the charge when every option is beyond the measured range. This is a deliberately mixed review set, not a population estimate.</li>
<li>Keep a small varied showcase shortlist and collect actual playtest feedback before changing existing daily puzzles.</li></ol>
<h2>Shortlist for human playtesting</h2><ul>{shortlist}</ul><p>Editorial candidates, not measured completion or enjoyment claims.</p><h2>Repeated endpoints</h2>{repeats}<h2>{len(cards)} reviewed routes</h2><p>{report['checked_shards']} article shards inspected. Showcase candidates need human playtesting; no public puzzles were changed.</p>{''.join(cards)}</html>'''


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--site', type=Path, default=Path('static_site'))
    ap.add_argument('--out', type=Path, default=Path('reports/puzzle-audit.html'))
    args = ap.parse_args()
    report = build_report(args.site)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(render_html(report))
    args.out.with_suffix('.json').write_text(json.dumps(report, indent=2))
    print(json.dumps({d: s for d, s in report['summaries'].items()}, indent=2))
    print(f"Reviewed {len(report['review'])} routes; {sum(bool(r['errors']) for r in report['review'])} failed. Report: {args.out.resolve()}")


if __name__ == '__main__':
    main()
