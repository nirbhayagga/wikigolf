//! Pass 1: build the article identity table.
//!
//! Every ns=0 page — article *and* redirect — gets a "raw id". Redirects are
//! then resolved (following chains) onto real articles, and only real articles
//! receive a dense "article id" 0..N-1, which is what the edge list uses.
//!
//! This is the step the previous pipeline lacked entirely, and its absence is
//! what produced 28M vertices for a ~7M article encyclopedia.

use crate::dump::{stream_pages, DumpStats, Page, Progress};
use crate::titles::normalize_title;
use anyhow::Result;
use rustc_hash::FxHashMap;
use std::io::BufRead;

pub const NONE: u32 = u32::MAX;
/// Redirect chains deeper than this are treated as broken. MediaWiki itself
/// only follows one hop; a couple more is generous without risking cycles.
const MAX_REDIRECT_HOPS: usize = 4;

pub struct TitleIndex {
    /// Normalized title -> raw id (articles and redirects alike).
    map: FxHashMap<Box<str>, u32>,
    /// Raw id -> normalized title.
    names: Vec<Box<str>>,
    is_redirect: Vec<bool>,
    /// Raw id -> raw id it redirects to, after chain resolution. NONE if the
    /// chain is broken, cyclic, or points outside ns=0.
    resolved: Vec<u32>,
    /// Raw id -> dense article id. NONE for redirects.
    article_id: Vec<u32>,
    pub n_articles: u32,
}

#[derive(Default, Debug)]
pub struct Pass1Stats {
    pub ns0_pages: u64,
    pub articles: u64,
    pub redirects: u64,
    pub broken_redirects: u64,
    pub duplicate_titles: u64,
    /// Raw titles that normalized onto an already-seen title, capped.
    ///
    /// MediaWiki guarantees titles are unique per namespace, so a collision
    /// means *our* normalization over-merged two distinct real articles — the
    /// `ß`/`SS` bug's signature. A count alone cannot be investigated, so keep
    /// examples. Full enwiki produces ~67 of these, and one of them silently
    /// deletes a real article, so they are worth being able to look at.
    pub collisions: Vec<(String, String)>,
}

/// Enough to diagnose a pattern without unbounded memory on a bad run.
const MAX_COLLISION_SAMPLES: usize = 50;

impl TitleIndex {
    /// Resolve a *normalized* link target to a dense article id, following
    /// redirects. Returns `None` for red links (targets that name no article).
    #[inline]
    pub fn lookup(&self, normalized: &str) -> Option<u32> {
        let raw = *self.map.get(normalized)?;
        let target = self.resolved[raw as usize];
        if target == NONE {
            return None;
        }
        let id = self.article_id[target as usize];
        if id == NONE {
            None
        } else {
            Some(id)
        }
    }

    /// Iterate `(article_id, title)` for every real article, in id order.
    pub fn articles(&self) -> impl Iterator<Item = (u32, &str)> {
        self.names
            .iter()
            .enumerate()
            .filter_map(move |(raw, name)| {
                let id = self.article_id[raw];
                if id == NONE {
                    None
                } else {
                    Some((id, &**name))
                }
            })
    }

    /// Iterate `(redirect_title, article_id)` for every redirect that resolves.
    /// Useful later for search-by-alias in the viewer and the pathfinder.
    pub fn redirects(&self) -> impl Iterator<Item = (&str, u32)> {
        self.names
            .iter()
            .enumerate()
            .filter_map(move |(raw, name)| {
                if !self.is_redirect[raw] {
                    return None;
                }
                let target = self.resolved[raw];
                if target == NONE {
                    return None;
                }
                let id = self.article_id[target as usize];
                if id == NONE {
                    None
                } else {
                    Some((&**name, id))
                }
            })
    }
}

pub fn build<R: BufRead>(input: R) -> Result<(TitleIndex, DumpStats, Pass1Stats)> {
    let mut map: FxHashMap<Box<str>, u32> = FxHashMap::default();
    let mut names: Vec<Box<str>> = Vec::new();
    let mut is_redirect: Vec<bool> = Vec::new();
    // Redirect targets can only be resolved once every title has been seen.
    let mut redirect_raw: Vec<Option<Box<str>>> = Vec::new();
    let mut st = Pass1Stats::default();
    let progress = Progress::new("pass 1:");

    let dump_stats = stream_pages(input, |p: &Page| {
        progress.tick(st.ns0_pages);
        if p.ns != 0 {
            return Ok(());
        }
        let Some(title) = normalize_title(&p.title) else {
            return Ok(());
        };
        st.ns0_pages += 1;

        let id = match map.get(title.as_str()) {
            Some(&id) => {
                st.duplicate_titles += 1;
                if st.collisions.len() < MAX_COLLISION_SAMPLES {
                    st.collisions.push((p.title.clone(), title.clone()));
                }
                id
            }
            None => {
                let id = names.len() as u32;
                let boxed: Box<str> = title.clone().into_boxed_str();
                names.push(boxed.clone());
                is_redirect.push(false);
                redirect_raw.push(None);
                map.insert(boxed, id);
                id
            }
        };

        match &p.redirect {
            Some(target) => {
                is_redirect[id as usize] = true;
                redirect_raw[id as usize] = normalize_title(target).map(|t| t.into_boxed_str());
                st.redirects += 1;
            }
            None => st.articles += 1,
        }
        Ok(())
    })?;
    progress.done(st.ns0_pages);

    let n = names.len();

    // Redirect target strings -> raw ids.
    let mut direct = vec![NONE; n];
    for (i, rt) in redirect_raw.iter().enumerate() {
        if let Some(s) = rt {
            if let Some(&tid) = map.get(&**s) {
                direct[i] = tid;
            }
        }
    }
    drop(redirect_raw);

    // Follow chains: A -> B -> C collapses to A -> C.
    let mut resolved = vec![NONE; n];
    for i in 0..n {
        if !is_redirect[i] {
            resolved[i] = i as u32;
            continue;
        }
        let mut cur = direct[i];
        let mut hops = 0;
        while cur != NONE && is_redirect[cur as usize] && hops < MAX_REDIRECT_HOPS {
            cur = direct[cur as usize];
            hops += 1;
        }
        if cur != NONE && !is_redirect[cur as usize] {
            resolved[i] = cur;
        } else {
            st.broken_redirects += 1;
        }
    }
    drop(direct);

    // Dense ids for real articles only.
    let mut article_id = vec![NONE; n];
    let mut next = 0u32;
    for i in 0..n {
        if !is_redirect[i] {
            article_id[i] = next;
            next += 1;
        }
    }

    Ok((
        TitleIndex {
            map,
            names,
            is_redirect,
            resolved,
            article_id,
            n_articles: next,
        },
        dump_stats,
        st,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const XML: &str = r#"<mediawiki>
<siteinfo><namespaces>
<namespace key="0" case="first-letter" />
<namespace key="6" case="first-letter">File</namespace>
<namespace key="14" case="first-letter">Category</namespace>
</namespaces></siteinfo>
<page><title>Anarchism</title><ns>0</ns><revision><text>x</text></revision></page>
<page><title>Political philosophy</title><ns>0</ns><revision><text>x</text></revision></page>
<page><title>AccessibleComputing</title><ns>0</ns><redirect title="Computer accessibility" /><revision><text>#REDIRECT</text></revision></page>
<page><title>Computer accessibility</title><ns>0</ns><revision><text>x</text></revision></page>
<page><title>A11y</title><ns>0</ns><redirect title="AccessibleComputing" /><revision><text>#REDIRECT</text></revision></page>
<page><title>Broken</title><ns>0</ns><redirect title="Does not exist" /><revision><text>#REDIRECT</text></revision></page>
<page><title>Talk:Anarchism</title><ns>1</ns><revision><text>x</text></revision></page>
</mediawiki>"#;

    fn idx() -> (TitleIndex, Pass1Stats) {
        let (i, _, s) = build(Cursor::new(XML)).unwrap();
        (i, s)
    }

    #[test]
    fn counts_articles_and_redirects() {
        let (i, s) = idx();
        assert_eq!(
            s.articles, 3,
            "Anarchism, Political philosophy, Computer accessibility"
        );
        assert_eq!(s.redirects, 3);
        assert_eq!(i.n_articles, 3);
    }

    #[test]
    fn skips_non_ns0() {
        let (i, _) = idx();
        assert!(i.lookup("Talk:Anarchism").is_none());
    }

    #[test]
    fn resolves_redirect_chain() {
        let (i, _) = idx();
        let direct = i.lookup("Computer accessibility").unwrap();
        assert_eq!(i.lookup("AccessibleComputing"), Some(direct));
        // A11y -> AccessibleComputing -> Computer accessibility
        assert_eq!(i.lookup("A11y"), Some(direct));
    }

    #[test]
    fn broken_redirect_is_not_a_node() {
        let (i, s) = idx();
        assert!(i.lookup("Broken").is_none());
        assert_eq!(s.broken_redirects, 1);
    }

    #[test]
    fn case_variants_resolve_to_one_article() {
        let (i, _) = idx();
        let a = i.lookup(&normalize_title("political philosophy").unwrap());
        let b = i.lookup(&normalize_title("Political_philosophy").unwrap());
        let c = i.lookup(&normalize_title("Political philosophy#History").unwrap());
        assert!(a.is_some());
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn red_links_are_rejected() {
        let (i, _) = idx();
        assert!(i.lookup("Nonexistent article").is_none());
    }
}
