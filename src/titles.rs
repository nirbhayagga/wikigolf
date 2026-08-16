//! Article title normalization, matching MediaWiki's own link-resolution rules.
//!
//! This is the module that decides whether two link strings refer to the same
//! article. Getting it wrong is what inflates the graph: without it,
//! `[[political philosophy]]`, `[[Political_philosophy]]` and
//! `[[Political philosophy#History]]` become three separate nodes.

use rustc_hash::FxHashSet;

/// Normalize a raw wikilink target or page title into MediaWiki's canonical form.
///
/// Returns `None` for targets that can never name an article (empty, pure
/// anchors like `[[#Section]]`).
pub fn normalize_title(raw: &str) -> Option<String> {
    let mut s = raw.trim();

    // `[[:Category:X]]` / `[[:en:X]]` — a leading colon forces a literal link.
    while let Some(rest) = s.strip_prefix(':') {
        s = rest.trim_start();
    }

    // Drop the section anchor: `Foo#History` and `Foo` are the same article.
    if let Some(i) = s.find('#') {
        s = &s[..i];
    }

    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Underscores are spaces, and runs of whitespace collapse to one space.
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for ch in s.chars() {
        let c = if ch == '_' { ' ' } else { ch };
        if c.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
        }
    }
    if out.is_empty() {
        return None;
    }

    // MediaWiki capitalizes the first character of every title in the main
    // namespace ($wgCapitalLinks), so `[[cat]]` and `[[Cat]]` are one article.
    //
    // Only single-character uppercase mappings are applied, which is what
    // MediaWiki's ucfirst does. Rust's `to_uppercase` follows full Unicode
    // case mapping, where 'ß' expands to "SS" — that would merge the real
    // article `ß` into the unrelated article `SS`.
    let mut chars = out.chars();
    let first = chars.next().unwrap();
    if first.is_lowercase() {
        let mut upper = first.to_uppercase();
        let u0 = upper.next().unwrap();
        if upper.next().is_none() {
            let rest = chars.as_str().to_string();
            out.clear();
            out.push(u0);
            out.push_str(&rest);
        }
    }

    Some(out)
}

/// Namespace prefixes declared by the dump's own `<siteinfo>` block.
///
/// This is only a fast-path filter: a target like `File:Foo.jpg` is not an
/// article, and skipping it here avoids a hash lookup. Correctness does not
/// depend on it — anything that is not a real article title gets dropped by the
/// red-link filter regardless. That is deliberate: a hand-maintained prefix
/// list would wrongly reject real articles such as `It: Chapter Two`.
pub struct NsPrefixes {
    set: FxHashSet<String>,
}

impl NsPrefixes {
    pub fn from_dump(names: &[String]) -> Self {
        let mut set = FxHashSet::default();
        for n in names {
            if !n.is_empty() {
                set.insert(n.to_lowercase());
                // Namespace names are also accepted with underscores for spaces.
                set.insert(n.to_lowercase().replace(' ', "_"));
            }
        }
        NsPrefixes { set }
    }

    /// True if `title` starts with a declared non-article namespace prefix.
    pub fn is_foreign(&self, title: &str) -> bool {
        match title.find(':') {
            Some(i) => self.set.contains(title[..i].trim().to_lowercase().as_str()),
            None => false,
        }
    }

    /// The category name in `[[Category:Living people]]`, or None.
    ///
    /// Categories are the only namespace worth keeping: they are
    /// human-authored topic labels sitting in the wikitext of every article,
    /// and they answer "what is this about" without storing a word of prose.
    ///
    /// A sort key after a pipe is dropped — `[[Category:Foo|Bar]]` files the
    /// article under Foo and only sorts it as "Bar".
    pub fn category(&self, title: &str) -> Option<String> {
        let i = title.find(':')?;
        if !title[..i].trim().eq_ignore_ascii_case("category") {
            return None;
        }
        let rest = title[i + 1..].split('|').next()?.trim();
        if rest.is_empty() {
            return None;
        }
        normalize_title(rest)
    }
}

/// Categories that describe the *edit state* of an article rather than its
/// subject. Wikipedia has thousands of them and they are on a large share of
/// articles, so leaving them in would bury the real topics under
/// "Articles with dead external links".
pub fn is_maintenance_category(name: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "Articles ", "All articles", "Wikipedia ", "CS1 ", "Webarchive",
        "Pages ", "All pages", "Use dmy dates", "Use mdy dates",
        "Short description", "Commons category", "Coordinates ",
        "Official website", "Good articles", "Featured articles",
        "Redirects ", "All redirects", "Template ", "Interlanguage ",
        "Harv and Sfn", "AC with ", "Vague or ambiguous",
    ];
    const CONTAINS: &[&str] = &[
        "maint:", "errors", "stub", "with unsourced", "needing", "lacking",
        "from ", "dead external links", "unreferenced", "cleanup",
    ];
    let lower = name.to_lowercase();
    PREFIXES.iter().any(|p| name.starts_with(p))
        || CONTAINS.iter().any(|c| lower.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Option<String> {
        normalize_title(s)
    }

    #[test]
    fn capitalizes_first_letter() {
        assert_eq!(n("political philosophy").unwrap(), "Political philosophy");
        assert_eq!(n("Political philosophy").unwrap(), "Political philosophy");
    }

    #[test]
    fn underscores_become_spaces() {
        assert_eq!(n("United_States").unwrap(), "United States");
        assert_eq!(n("United   States").unwrap(), "United States");
    }

    #[test]
    fn strips_anchors() {
        assert_eq!(n("Anarchism#History").unwrap(), "Anarchism");
        assert_eq!(n("#Section"), None);
    }

    #[test]
    fn strips_leading_colon() {
        assert_eq!(n(":Category:Foo").unwrap(), "Category:Foo");
    }

    #[test]
    fn keeps_colons_inside_real_titles() {
        // The old parser dropped every title containing a colon, silently
        // deleting tens of thousands of real articles.
        assert_eq!(
            n("Star Trek: The Next Generation").unwrap(),
            "Star Trek: The Next Generation"
        );
    }

    #[test]
    fn does_not_expand_multi_char_uppercase() {
        // 'ß'.to_uppercase() is "SS" under full Unicode case mapping, which
        // would collide the article `ß` with the unrelated article `SS`.
        // Both exist on Simple English Wikipedia.
        assert_eq!(n("ß").unwrap(), "ß");
        assert_eq!(n("SS").unwrap(), "SS");
        assert_ne!(n("ß").unwrap(), n("SS").unwrap());
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(n(""), None);
        assert_eq!(n("   "), None);
        assert_eq!(n("_"), None);
    }

    #[test]
    fn ns_prefixes_are_case_insensitive() {
        let ns = NsPrefixes::from_dump(&["File".into(), "Category".into(), "Talk".into()]);
        assert!(ns.is_foreign("File:Example.jpg"));
        assert!(ns.is_foreign("category:Living people"));
        assert!(!ns.is_foreign("It: Chapter Two"));
        assert!(!ns.is_foreign("Anarchism"));
    }
}
