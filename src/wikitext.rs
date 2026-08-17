//! Wikitext cleaning and `[[wikilink]]` extraction.
//!
//! What counts as a "link" is the main editorial decision in this project, so
//! every removal is behind a flag and documented here:
//!
//! * HTML comments and `<nowiki>` are **always** removed — they do not render,
//!   so links inside them are not links a reader could ever click.
//! * `{{templates}}` are **kept by default**. Note this is a smaller change
//!   than it sounds: navboxes are transcluded, so in raw wikitext they appear
//!   as a bare `{{US Presidents}}` with no links inside. What keeping templates
//!   actually admits is infobox parameter links (`| capital = [[Paris]]`).
//! * `<ref>` citation bodies are kept by default (`--strip-refs` to drop).
//! * Citation sections (References / External links / ...) are cut by default.
//!   `See also` is deliberately **kept**: those are editorially curated
//!   related-article links, which is exactly the relation this graph is about.

use memchr::memmem;
use regex::Regex;

#[derive(Clone, Copy)]
pub struct CleanOpts {
    pub strip_templates: bool,
    pub strip_refs: bool,
    pub cut_citation_sections: bool,
}

impl Default for CleanOpts {
    fn default() -> Self {
        CleanOpts { strip_templates: false, strip_refs: false, cut_citation_sections: true }
    }
}

pub struct Cleaner {
    a: String,
    b: String,
    sections: Regex,
}

impl Cleaner {
    pub fn new() -> Self {
        // The old parser used `split("== References ==")`, which missed the
        // (far more common) `==References==` and every casing variant.
        let sections = Regex::new(
            r"(?mi)^[ \t]*={2,}[ \t]*(references|external links|further reading|notes|bibliography|sources|citations|footnotes)[ \t]*={2,}[ \t]*$",
        )
        .expect("static regex");
        Cleaner { a: String::new(), b: String::new(), sections }
    }

    pub fn clean<'s>(&'s mut self, src: &'s str, opts: &CleanOpts) -> &'s str {
        strip_comments(src, &mut self.a);

        strip_element(&self.a, "nowiki", &mut self.b);
        std::mem::swap(&mut self.a, &mut self.b);

        if opts.strip_refs {
            strip_element(&self.a, "ref", &mut self.b);
            std::mem::swap(&mut self.a, &mut self.b);
        }

        if opts.strip_templates {
            strip_templates(&self.a, &mut self.b);
            std::mem::swap(&mut self.a, &mut self.b);
        }

        if opts.cut_citation_sections {
            if let Some(m) = self.sections.find(&self.a) {
                self.a.truncate(m.start());
            }
        }

        &self.a
    }
}

impl Default for Cleaner {
    fn default() -> Self {
        Self::new()
    }
}

/// Remove `<!-- ... -->`. An unterminated comment eats the remainder.
fn strip_comments(src: &str, out: &mut String) {
    out.clear();
    let b = src.as_bytes();
    let mut i = 0;
    while let Some(rel) = memmem::find(&b[i..], b"<!--") {
        let start = i + rel;
        out.push_str(&src[i..start]);
        match memmem::find(&b[start + 4..], b"-->") {
            Some(r) => i = start + 4 + r + 3,
            None => return,
        }
    }
    out.push_str(&src[i..]);
}

/// Remove `<tag ...>...</tag>` and self-closing `<tag ... />`, case-insensitively.
fn strip_element(src: &str, tag: &str, out: &mut String) {
    out.clear();
    let b = src.as_bytes();
    let close: Vec<u8> = format!("</{tag}").into_bytes();
    let mut i = 0;

    while i < b.len() {
        let Some(start) = find_open_tag(b, i, tag) else { break };
        out.push_str(&src[i..start]);

        // Find the end of the opening tag.
        let Some(gt_rel) = memchr::memchr(b'>', &b[start..]) else { return };
        let gt = start + gt_rel;
        let self_closing = b[..gt].iter().rev().find(|c| !c.is_ascii_whitespace()) == Some(&b'/');

        if self_closing {
            i = gt + 1;
            continue;
        }

        match find_ci(b, gt + 1, &close) {
            Some(c) => match memchr::memchr(b'>', &b[c..]) {
                Some(e) => i = c + e + 1,
                None => return,
            },
            None => return, // unterminated: drop the rest
        }
    }
    out.push_str(&src[i..]);
}

/// Find `<tag` at or after `from`, requiring a delimiter after the tag name so
/// that `<ref>` matches but `<references/>` does not.
fn find_open_tag(b: &[u8], from: usize, tag: &str) -> Option<usize> {
    let needle = format!("<{tag}");
    let n = needle.as_bytes();
    let mut i = from;
    loop {
        let at = find_ci(b, i, n)?;
        let after = at + n.len();
        match b.get(after) {
            Some(c) if c.is_ascii_whitespace() || *c == b'>' || *c == b'/' => return Some(at),
            None => return None,
            _ => i = at + 1,
        }
    }
}

/// ASCII case-insensitive substring search.
fn find_ci(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() {
        return None;
    }
    let first = needle[0].to_ascii_lowercase();
    let mut i = from;
    while i + needle.len() <= hay.len() {
        if hay[i].to_ascii_lowercase() == first
            && hay[i..i + needle.len()]
                .iter()
                .zip(needle)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Remove `{{ ... }}`, honouring nesting. Tables (`{| ... |}`) are kept.
fn strip_templates(src: &str, out: &mut String) {
    out.clear();
    let b = src.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    let mut kept_from = 0usize;

    while i + 1 < b.len() {
        if b[i] == b'{' && b[i + 1] == b'{' {
            if depth == 0 {
                out.push_str(&src[kept_from..i]);
            }
            depth += 1;
            i += 2;
        } else if b[i] == b'}' && b[i + 1] == b'}' && depth > 0 {
            depth -= 1;
            i += 2;
            if depth == 0 {
                kept_from = i;
            }
        } else {
            i += 1;
        }
    }
    if depth == 0 {
        out.push_str(&src[kept_from..]);
    }
}

/// Call `f` with the raw target of every `[[wikilink]]` in `text`.
///
/// Only the target is reported; display text after `|` is discarded. Nested
/// constructs such as `[[File:x.jpg|thumb|see [[Paris]]]]` yield the inner
/// link, which is the one a reader can actually click.
pub fn for_each_link(text: &str, mut f: impl FnMut(&str)) {
    let b = text.as_bytes();
    let mut i = 0;

    while let Some(rel) = memmem::find(&b[i..], b"[[") {
        let open = i + rel;
        let body_start = open + 2;

        let Some(close_rel) = memmem::find(&b[body_start..], b"]]") else { return };
        let close = body_start + close_rel;

        // If another `[[` opens before this one closes, the inner link is the
        // real one — restart there rather than swallowing it.
        if let Some(inner_rel) = memmem::find(&b[body_start..close], b"[[") {
            i = body_start + inner_rel;
            continue;
        }

        let body = &text[body_start..close];
        let target = match body.find('|') {
            Some(p) => &body[..p],
            None => body,
        };
        if !target.is_empty() {
            f(target);
        }
        i = close + 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(s: &str) -> Vec<String> {
        let mut v = Vec::new();
        for_each_link(s, |l| v.push(l.to_string()));
        v
    }

    #[test]
    fn extracts_plain_and_piped_links() {
        assert_eq!(links("see [[Cat]] and [[Dog|dogs]]."), vec!["Cat", "Dog"]);
    }

    #[test]
    fn extracts_inner_link_from_image_caption() {
        assert_eq!(links("[[File:a.jpg|thumb|of [[Paris]]]]"), vec!["Paris"]);
    }

    #[test]
    fn ignores_unclosed_link() {
        assert_eq!(links("[[Cat]] and [[Dog"), vec!["Cat"]);
    }

    #[test]
    fn comments_are_removed() {
        let mut out = String::new();
        strip_comments("a <!-- [[Hidden]] --> b", &mut out);
        assert_eq!(out, "a  b");
        assert!(links(&out).is_empty());
    }

    #[test]
    fn templates_strip_with_nesting() {
        let mut out = String::new();
        strip_templates("a {{infobox|x={{inner|[[Deep]]}}|y=[[Mid]]}} b", &mut out);
        assert_eq!(out, "a  b");
    }

    #[test]
    fn tables_survive_template_stripping() {
        let mut out = String::new();
        strip_templates("{| class=x\n| [[Cell]]\n|}", &mut out);
        assert!(out.contains("[[Cell]]"));
    }

    #[test]
    fn refs_stripped_only_on_request() {
        let mut out = String::new();
        strip_element("a <ref>see [[Book]]</ref> b <ref name=\"x\" /> c", "ref", &mut out);
        assert_eq!(out, "a  b  c");
    }

    #[test]
    fn nowiki_is_removed() {
        let mut out = String::new();
        strip_element("a <nowiki>[[Not a link]]</nowiki> b", "nowiki", &mut out);
        assert!(links(&out).is_empty());
    }

    #[test]
    fn cuts_citation_sections_in_any_casing() {
        let mut c = Cleaner::new();
        let opts = CleanOpts::default();
        let src = "Body [[Keep]]\n==References==\n[[Drop]]\n";
        assert_eq!(links(c.clean(src, &opts)), vec!["Keep"]);

        let src2 = "Body [[Keep]]\n== External Links ==\n[[Drop]]\n";
        assert_eq!(links(c.clean(src2, &opts)), vec!["Keep"]);
    }

    #[test]
    fn see_also_is_kept() {
        let mut c = Cleaner::new();
        let src = "Body [[A]]\n== See also ==\n[[B]]\n";
        assert_eq!(links(c.clean(src, &CleanOpts::default())), vec!["A", "B"]);
    }

    #[test]
    fn templates_kept_by_default() {
        let mut c = Cleaner::new();
        let src = "{{Infobox country|capital=[[Paris]]}} Body [[France]]";
        assert_eq!(links(c.clean(src, &CleanOpts::default())), vec!["Paris", "France"]);
    }
}
