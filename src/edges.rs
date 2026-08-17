//! Pass 2: emit the integer edge list.
//!
//! Every link target is normalized, resolved through redirects, and dropped if
//! it does not name a real article (a "red link"). Duplicate targets are
//! removed per page — which is exact global deduplication, because the edge
//! (A, B) can only ever be produced by page A.

use crate::dump::{stream_pages, Page, Progress};
use crate::index::TitleIndex;
use crate::output::{CategoryWriter, EdgeWriter};
use crate::titles::{is_maintenance_category, normalize_title, NsPrefixes};
use crate::wikitext::{for_each_link, CleanOpts, Cleaner};
use anyhow::Result;
use std::io::BufRead;
use std::path::Path;

#[derive(Default, Debug)]
pub struct Pass2Stats {
    pub pages_scanned: u64,
    pub links_seen: u64,
    pub skipped_namespace: u64,
    pub red_links: u64,
    pub self_links: u64,
    pub duplicate_links: u64,
    pub duplicate_pages: u64,
    pub edges_written: u64,
    pub categories_written: u64,
    pub categories_skipped_maintenance: u64,
}

/// Categories kept per article, in the order the wikitext lists them.
///
/// Articles carry a long tail of increasingly specific categories; the first
/// few are the ones an editor considered defining, and they are what a player
/// needs to know what an article is. Keeping all of them would multiply the
/// output several times over for labels nobody reads.
const MAX_CATEGORIES_PER_ARTICLE: usize = 6;

/// The v3 template extractions, gathered in the same pass as everything
/// else — nothing is worth a second traversal of a 27 GB dump.
#[derive(Default)]
pub struct ExtrasOut {
    /// Sparse (id, one-line gloss) from {{Short description}}.
    pub descs: Vec<(u32, String)>,
    /// Sparse (id, infobox kind), e.g. "person", "film".
    pub kinds: Vec<(u32, String)>,
    /// Dense per-article bitmask: see extras::FLAG_*.
    pub flags: Vec<u32>,
    /// Sparse (id, lat, lon) from {{coord}}.
    pub coords: Vec<(u32, f32, f32)>,
}

pub fn build<R: BufRead>(
    input: R,
    idx: &TitleIndex,
    ns: &NsPrefixes,
    opts: &CleanOpts,
    out_path: &Path,
    cats_path: &Path,
) -> Result<(Pass2Stats, Vec<u32>, ExtrasOut)> {
    let mut writer = EdgeWriter::create(out_path)?;
    let mut cats = CategoryWriter::create(cats_path)?;
    // Wikitext byte length per article, indexed by dense id. The dump hands it
    // over for free while the text is already in memory.
    let mut sizes = vec![0u32; idx.n_articles as usize];
    let mut extras = ExtrasOut { flags: vec![0u32; idx.n_articles as usize], ..Default::default() };
    let mut page_cats: Vec<String> = Vec::with_capacity(16);
    let mut cleaner = Cleaner::new();
    let mut targets: Vec<u32> = Vec::with_capacity(4096);
    let mut st = Pass2Stats::default();
    let progress = Progress::new("pass 2:");

    // Deduplicating targets within a page is exact global deduplication only
    // as long as each article is emitted once. Two <page> elements can share a
    // normalized title, so enforce it rather than assume it.
    let mut emitted = vec![false; idx.n_articles as usize];

    stream_pages(input, |p: &Page| {
        if p.ns != 0 || p.redirect.is_some() {
            return Ok(());
        }
        let Some(title) = normalize_title(&p.title) else {
            return Ok(());
        };
        let Some(src) = idx.lookup(&title) else {
            return Ok(());
        };
        if emitted[src as usize] {
            st.duplicate_pages += 1;
            return Ok(());
        }
        emitted[src as usize] = true;

        st.pages_scanned += 1;
        progress.tick(st.pages_scanned);

        targets.clear();
        page_cats.clear();
        sizes[src as usize] = p.text.len().min(u32::MAX as usize) as u32;

        // Template-borne metadata, from the same raw text and for the same
        // reason as categories: cleaning would truncate it away.
        let ex = crate::extras::extract(&p.text, &title);
        if let Some(d) = ex.description {
            extras.descs.push((src, d));
        }
        if let Some(k) = ex.kind {
            extras.kinds.push((src, k));
        }
        extras.flags[src as usize] = ex.flags;
        if let Some((lat, lon)) = ex.coord {
            extras.coords.push((src, lat, lon));
        }

        // Categories come from the RAW wikitext, not the cleaned text.
        //
        // Cleaning truncates the article at the first citation section, and
        // categories sit below those sections at the very bottom of the page.
        // Reading them from the cleaned text lost them for every article with
        // a References heading — measured at 70% of Simple English, including
        // United States, Cat and Albert Einstein.
        //
        // They are metadata rather than body links, so the flags that decide
        // what counts as a *link* have no business deciding what counts as a
        // category either.
        for_each_link(&p.text, |raw| {
            if page_cats.len() >= MAX_CATEGORIES_PER_ARTICLE {
                return;
            }
            if let Some(c) = ns.category(raw) {
                if is_maintenance_category(&c) {
                    st.categories_skipped_maintenance += 1;
                } else if !page_cats.contains(&c) {
                    page_cats.push(c);
                }
            }
        });

        let cleaned = cleaner.clean(&p.text, opts);

        for_each_link(cleaned, |raw| {
            st.links_seen += 1;

            if ns.is_foreign(raw) {
                st.skipped_namespace += 1;
                return;
            }

            // Most targets are already in canonical form, so try them as-is
            // before paying for a normalization allocation.
            let dst = match idx.lookup(raw) {
                Some(d) => Some(d),
                None => match normalize_title(raw) {
                    Some(n) => idx.lookup(&n),
                    None => None,
                },
            };

            match dst {
                Some(d) if d == src => st.self_links += 1,
                Some(d) => targets.push(d),
                None => st.red_links += 1,
            }
        });

        let before = targets.len();
        targets.sort_unstable();
        targets.dedup();
        st.duplicate_links += (before - targets.len()) as u64;

        for c in page_cats.iter() {
            cats.push(src, c)?;
            st.categories_written += 1;
        }

        for &dst in targets.iter() {
            writer.push(src, dst)?;
        }
        Ok(())
    })?;

    progress.done(st.pages_scanned);
    st.edges_written = writer.finish()?;
    st.categories_written = cats.finish()?;
    Ok((st, sizes, extras))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index;
    use std::io::Cursor;

    const XML: &str = r#"<mediawiki>
<siteinfo><namespaces>
<namespace key="0" case="first-letter" />
<namespace key="6" case="first-letter">File</namespace>
<namespace key="14" case="first-letter">Category</namespace>
</namespaces></siteinfo>
<page><title>Anarchism</title><ns>0</ns><revision><text>
[[political philosophy]] and [[Political philosophy]] and [[Political_philosophy#History]].
[[Computer accessibility]] plus the alias [[AccessibleComputing]].
[[File:Flag.jpg|thumb|caption]] [[Category:Politics]]
[[Nonexistent page]] [[Anarchism]]
&lt;!-- [[Hidden]] --&gt;
{{Infobox|field=[[Star Trek: The Next Generation]]}}
== References ==
[[Political philosophy]]
</text></revision></page>
<page><title>Political philosophy</title><ns>0</ns><revision><text>[[Anarchism]]</text></revision></page>
<page><title>Computer accessibility</title><ns>0</ns><revision><text>no links</text></revision></page>
<page><title>Star Trek: The Next Generation</title><ns>0</ns><revision><text>[[Anarchism]]</text></revision></page>
<page><title>AccessibleComputing</title><ns>0</ns><redirect title="Computer accessibility" /><revision><text>#REDIRECT</text></revision></page>
</mediawiki>"#;

    fn run(opts: CleanOpts) -> (Pass2Stats, index::TitleIndex, Vec<(u32, u32)>) {
        let (idx, dump, _) = index::build(Cursor::new(XML)).unwrap();
        let ns = NsPrefixes::from_dump(&dump.namespaces);
        // Tests run in parallel threads, so each needs its own output file.
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("wpe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("edges-{seq}.parquet"));
        let cpath = dir.join(format!("cats-{seq}.parquet"));
        let (st, _sizes, _extras) =
            build(Cursor::new(XML), &idx, &ns, &opts, &path, &cpath).unwrap();
        let edges = read_back(&path);
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&cpath).ok();
        (st, idx, edges)
    }

    /// Run pass 2 and hand back what it recorded besides edges.
    fn run_extras(opts: CleanOpts) -> (Pass2Stats, Vec<(u32, String)>, Vec<u32>) {
        let (idx, dump, _) = index::build(Cursor::new(XML)).unwrap();
        let ns = NsPrefixes::from_dump(&dump.namespaces);
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1000);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("wpe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("edges-{seq}.parquet"));
        let cpath = dir.join(format!("cats-{seq}.parquet"));
        let (st, sizes, _extras) =
            build(Cursor::new(XML), &idx, &ns, &opts, &path, &cpath).unwrap();

        use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
        let file = std::fs::File::open(&cpath).unwrap();
        let reader = ParquetRecordBatchReaderBuilder::try_new(file).unwrap().build().unwrap();
        let mut cats = Vec::new();
        for batch in reader {
            let batch = batch.unwrap();
            let ids = batch
                .column(0)
                .as_any()
                .downcast_ref::<arrow::array::UInt32Array>()
                .unwrap();
            let names = batch
                .column(1)
                .as_any()
                .downcast_ref::<arrow::array::StringArray>()
                .unwrap();
            for i in 0..batch.num_rows() {
                cats.push((ids.value(i), names.value(i).to_string()));
            }
        }
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&cpath).ok();
        (st, cats, sizes)
    }

    #[test]
    fn categories_are_kept_and_edges_are_not_affected() {
        let (st, cats, _) = run_extras(CleanOpts::default());
        // The fixture's only category link is [[Category:Politics]].
        assert!(
            cats.iter().any(|(_, c)| c == "Politics"),
            "expected Politics among {cats:?}"
        );
        // Still counted as a skipped namespace link — categories are collected
        // in addition to being excluded from the graph, never instead.
        assert!(st.skipped_namespace >= 1);
        assert_eq!(st.categories_written as usize, cats.len());
    }

    #[test]
    fn article_sizes_are_recorded_per_id() {
        let (_, _, sizes) = run_extras(CleanOpts::default());
        assert!(!sizes.is_empty());
        assert!(
            sizes.iter().any(|&b| b > 0),
            "every article in the fixture has wikitext, so some size must be non-zero"
        );
    }

    fn read_back(path: &Path) -> Vec<(u32, u32)> {
        use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
        let file = std::fs::File::open(path).unwrap();
        let reader = ParquetRecordBatchReaderBuilder::try_new(file).unwrap().build().unwrap();
        let mut out = Vec::new();
        for batch in reader {
            let batch = batch.unwrap();
            let s = batch
                .column(0)
                .as_any()
                .downcast_ref::<arrow::array::Int32Array>()
                .unwrap();
            let d = batch
                .column(1)
                .as_any()
                .downcast_ref::<arrow::array::Int32Array>()
                .unwrap();
            for i in 0..batch.num_rows() {
                out.push((s.value(i) as u32, d.value(i) as u32));
            }
        }
        out
    }

    #[test]
    fn case_and_underscore_variants_collapse_to_one_edge() {
        let (st, idx, edges) = run(CleanOpts::default());
        let a = idx.lookup("Anarchism").unwrap();
        let pp = idx.lookup("Political philosophy").unwrap();
        assert_eq!(
            edges.iter().filter(|&&(s, d)| s == a && d == pp).count(),
            1,
            "three spellings of the same article must produce exactly one edge"
        );
        assert!(st.duplicate_links >= 2);
    }

    #[test]
    fn redirect_alias_maps_to_its_target() {
        let (_, idx, edges) = run(CleanOpts::default());
        let a = idx.lookup("Anarchism").unwrap();
        let ca = idx.lookup("Computer accessibility").unwrap();
        assert_eq!(edges.iter().filter(|&&(s, d)| s == a && d == ca).count(), 1);
    }

    #[test]
    fn red_links_and_namespaces_are_dropped() {
        let (st, _, _) = run(CleanOpts::default());
        assert_eq!(st.red_links, 1, "[[Nonexistent page]]");
        assert_eq!(st.skipped_namespace, 2, "File: and Category:");
    }

    #[test]
    fn self_links_are_dropped() {
        let (st, idx, edges) = run(CleanOpts::default());
        let a = idx.lookup("Anarchism").unwrap();
        assert_eq!(st.self_links, 1);
        assert!(!edges.iter().any(|&(s, d)| s == d), "no self loops");
        assert!(edges.iter().any(|&(s, _)| s == a));
    }

    #[test]
    fn colon_titles_survive() {
        let (_, idx, edges) = run(CleanOpts::default());
        let a = idx.lookup("Anarchism").unwrap();
        let st_ng = idx.lookup("Star Trek: The Next Generation").unwrap();
        assert!(
            edges.contains(&(a, st_ng)),
            "articles with colons are real articles, not namespaces"
        );
    }

    #[test]
    fn citation_section_links_are_cut() {
        // The only link in == References == is a duplicate, so the observable
        // effect is on the raw link count, not the edge set.
        let with = run(CleanOpts { cut_citation_sections: false, ..Default::default() }).0;
        let without = run(CleanOpts::default()).0;
        assert!(without.links_seen < with.links_seen);
    }

    #[test]
    fn stripping_templates_removes_infobox_links() {
        let (_, idx, edges) = run(CleanOpts { strip_templates: true, ..Default::default() });
        let a = idx.lookup("Anarchism").unwrap();
        let st_ng = idx.lookup("Star Trek: The Next Generation").unwrap();
        assert!(!edges.contains(&(a, st_ng)));
    }

    #[test]
    fn edges_reference_only_real_articles() {
        let (_, idx, edges) = run(CleanOpts::default());
        for (s, d) in edges {
            assert!(s < idx.n_articles && d < idx.n_articles);
        }
    }
}
