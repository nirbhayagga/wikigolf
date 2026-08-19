//! Streaming reader for MediaWiki XML dumps.
//!
//! Yields one `Page` at a time with buffers reused across pages, so memory is
//! O(largest article) rather than O(dump). Parse errors are fatal and reported
//! with a byte offset — a silently truncated pass over a 25 GB dump is worse
//! than a crash, because the resulting graph looks plausible but is incomplete.

use anyhow::{anyhow, Result};
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::io::BufRead;

/// Sentinel for "no <ns> element seen on this page".
pub const NO_NS: i64 = i64::MIN;

pub struct Page {
    pub title: String,
    pub ns: i64,
    /// `Some(target)` if this page is a `#REDIRECT`.
    pub redirect: Option<String>,
    pub text: String,
}

impl Default for Page {
    fn default() -> Self {
        Page {
            title: String::new(),
            ns: NO_NS,
            redirect: None,
            text: String::with_capacity(128 * 1024),
        }
    }
}

impl Page {
    fn reset(&mut self) {
        self.title.clear();
        self.ns = NO_NS;
        self.redirect = None;
        self.text.clear();
    }
}

#[derive(Default)]
pub struct DumpStats {
    pub pages: u64,
    /// Namespace names declared in <siteinfo>, e.g. "Talk", "File", "Category".
    pub namespaces: Vec<String>,
    /// Entity references we could not resolve (should be 0 on a real dump).
    pub unresolved_entities: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sink {
    None,
    Title,
    Ns,
    Text,
    NsName,
}

/// Resolve an XML entity reference body (the part between `&` and `;`).
fn resolve_entity(name: &str, out: &mut String) -> bool {
    if let Some(rest) = name.strip_prefix('#') {
        let cp = if let Some(hex) = rest.strip_prefix('x').or_else(|| rest.strip_prefix('X')) {
            u32::from_str_radix(hex, 16).ok()
        } else {
            rest.parse::<u32>().ok()
        };
        if let Some(c) = cp.and_then(char::from_u32) {
            out.push(c);
            return true;
        }
        return false;
    }
    match resolve_predefined_entity(name) {
        Some(s) => {
            out.push_str(s);
            true
        }
        None => false,
    }
}

/// Stream every `<page>` in a MediaWiki XML dump, calling `on_page` for each.
pub fn stream_pages<R, F>(input: R, mut on_page: F) -> Result<DumpStats>
where
    R: BufRead,
    F: FnMut(&Page) -> Result<()>,
{
    let mut reader = Reader::from_reader(input);
    // Wikitext is whitespace-significant (headings start a line, list items
    // start with `*`), so trimming would corrupt link context.
    reader.config_mut().trim_text(false);

    let mut buf = Vec::with_capacity(64 * 1024);
    let mut page = Page::default();
    let mut stats = DumpStats::default();
    let mut sink = Sink::None;
    let mut in_page = false;
    let mut in_revision = false;
    // Scratch for short elements (<ns>, <namespace>) that are not part of Page.
    let mut scratch = String::new();

    macro_rules! push_str {
        ($s:expr) => {
            match sink {
                Sink::Title => page.title.push_str($s),
                Sink::Text => page.text.push_str($s),
                Sink::Ns | Sink::NsName => scratch.push_str($s),
                Sink::None => {}
            }
        };
    }

    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"page" => {
                    in_page = true;
                    page.reset();
                }
                b"revision" => in_revision = true,
                b"title" if in_page => sink = Sink::Title,
                b"ns" if in_page => {
                    sink = Sink::Ns;
                    scratch.clear();
                }
                b"text" if in_revision => sink = Sink::Text,
                b"namespace" => {
                    sink = Sink::NsName;
                    scratch.clear();
                }
                b"redirect" if in_page => {
                    if let Some(a) = e.try_get_attribute("title")? {
                        page.redirect = Some(
                            a.normalized_value(quick_xml::XmlVersion::Explicit1_0)?
                                .into_owned(),
                        );
                    }
                }
                _ => {}
            },

            // <redirect title="..."/> and <namespace key="0"/> are empty elements.
            Ok(Event::Empty(e)) => match e.local_name().as_ref() {
                b"redirect" if in_page => {
                    if let Some(a) = e.try_get_attribute("title")? {
                        page.redirect = Some(
                            a.normalized_value(quick_xml::XmlVersion::Explicit1_0)?
                                .into_owned(),
                        );
                    }
                }
                b"text" if in_revision => {} // <text bytes="0" />: no content
                _ => {}
            },

            Ok(Event::Text(e)) => {
                if sink != Sink::None {
                    let s = e.xml10_content()?;
                    push_str!(&s);
                }
            }

            Ok(Event::CData(e)) => {
                if sink != Sink::None {
                    let s = e.decode()?;
                    push_str!(&s);
                }
            }

            // quick-xml >= 0.32 reports entity references as their own event
            // rather than folding them into the surrounding text. Ignoring
            // these silently deletes every `&` from titles ("AT&T" -> "ATT"),
            // which then fails every lookup and becomes a phantom node.
            Ok(Event::GeneralRef(e)) => {
                if sink != Sink::None {
                    let name = e.decode()?;
                    let mut resolved = String::new();
                    if resolve_entity(&name, &mut resolved) {
                        push_str!(&resolved);
                    } else {
                        stats.unresolved_entities += 1;
                        push_str!(&format!("&{};", name));
                    }
                }
            }

            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"title" | b"text" => sink = Sink::None,
                b"ns" => {
                    page.ns = scratch.trim().parse().unwrap_or(NO_NS);
                    sink = Sink::None;
                }
                b"namespace" => {
                    let n = scratch.trim();
                    if !n.is_empty() {
                        stats.namespaces.push(n.to_string());
                    }
                    sink = Sink::None;
                }
                b"revision" => in_revision = false,
                b"page" => {
                    in_page = false;
                    stats.pages += 1;
                    on_page(&page)?;
                }
                _ => {}
            },

            Ok(Event::Eof) => break,
            Ok(_) => {}

            Err(e) => {
                return Err(anyhow!(
                    "XML parse error at byte {}: {e}. The dump is likely truncated \
                     or corrupt — re-download and verify against the published checksums.",
                    reader.buffer_position()
                ))
            }
        }
    }

    Ok(stats)
}

/// Throttled progress line on stderr.
pub struct Progress {
    label: &'static str,
    start: std::time::Instant,
    every: u64,
}

impl Progress {
    pub fn new(label: &'static str) -> Self {
        Progress {
            label,
            start: std::time::Instant::now(),
            every: 100_000,
        }
    }

    pub fn tick(&self, n: u64) {
        if n.is_multiple_of(self.every) {
            let secs = self.start.elapsed().as_secs_f64();
            let rate = if secs > 0.0 { n as f64 / secs } else { 0.0 };
            eprint!("\r   {} {:>10} pages  ({:.0}/s)", self.label, n, rate);
        }
    }

    pub fn done(&self, n: u64) {
        let secs = self.start.elapsed().as_secs_f64();
        eprintln!(
            "\r   {} {:>10} pages  in {:.1}s        ",
            self.label, n, secs
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_entities() {
        let mut s = String::new();
        assert!(resolve_entity("amp", &mut s));
        assert_eq!(s, "&");
        s.clear();
        assert!(resolve_entity("#38", &mut s));
        assert_eq!(s, "&");
        s.clear();
        assert!(resolve_entity("#x2014", &mut s));
        assert_eq!(s, "—");
    }
}
