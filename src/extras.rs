//! The v3 extractions: what a page's templates say about the page.
//!
//! One scan over the RAW wikitext (cleaning truncates at citation sections
//! and would lose bottom-of-page templates, the same lesson categories
//! taught) collecting five things:
//!
//!   1. `{{Short description|...}}` — the editor-written one-line gloss.
//!   2. Disambiguation templates — flags pages that are forks, not articles.
//!   3. The first `{{Infobox X` name — a rough article kind.
//!   4. `{{coord|...}}` — real-world coordinates, preferring display=title.
//!   5. `{{Featured article}}` / `{{Good article}}` — editor-vetted quality.
//!
//! All heuristic by nature: templates are free-form and the long tail is
//! endless. Every consumer treats absence as "unknown", so a missed
//! extraction degrades to exactly the behaviour before v3 existed.

/// Article quality/kind flags, stored as a bitmask in article_flags.parquet.
pub const FLAG_DISAMBIG: u32 = 1;
pub const FLAG_FEATURED: u32 = 2;
pub const FLAG_GOOD: u32 = 4;

const MAX_DESC: usize = 160;
const MAX_KIND: usize = 40;

#[derive(Default, Debug, PartialEq)]
pub struct Extras {
    pub description: Option<String>,
    pub kind: Option<String>,
    pub flags: u32,
    pub coord: Option<(f32, f32)>,
}

/// Scan raw wikitext for the template-borne metadata above.
///
/// `title` participates because " (disambiguation)" in the title is a
/// stronger disambig signal than any template.
pub fn extract(text: &str, title: &str) -> Extras {
    let mut out = Extras::default();
    if title.ends_with(" (disambiguation)") {
        out.flags |= FLAG_DISAMBIG;
    }

    // Track whether the kept coord came from a display=title template — the
    // page's canonical location — so a later title-coord can replace an
    // earlier incidental one, but never the other way round.
    let mut coord_is_title = false;

    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(off) = memchr::memmem::find(&bytes[i..], b"{{") {
        let start = i + off + 2;
        i = start; // continue the outer scan from just past this opener
        let Some(rest) = text.get(start..) else { break };

        // Template name: up to '|' or '}}', trimmed, ASCII-lowercased.
        // Non-ASCII template names exist but none of the five targets use
        // them. Over-long names are SKIPPED, never sliced: clamping a byte
        // index at 64 once panicked 53 minutes into enwiki when an en-dash
        // straddled exactly that byte — `find` returns char boundaries,
        // arbitrary clamps do not.
        let name_end = match rest.find(['|', '}']) {
            Some(e) if e > 0 && e <= 64 => e,
            _ => continue,
        };
        let name_raw = rest[..name_end].trim();
        if name_raw.is_empty() {
            continue;
        }
        let name = name_raw.to_ascii_lowercase();
        let body = &rest[name_end..];

        match name.as_str() {
            "short description" | "shortdesc" | "short desc" => {
                if out.description.is_none() {
                    if let Some(arg) = first_arg(body) {
                        // "none" is the editors' way of saying "no gloss".
                        if !arg.is_empty() && !arg.eq_ignore_ascii_case("none") {
                            out.description = Some(truncate(arg, MAX_DESC));
                        }
                    }
                }
            }
            "coord" | "coor" => {
                if !coord_is_title {
                    if let Some((lat, lon, is_title)) = parse_coord(body) {
                        if out.coord.is_none() || is_title {
                            out.coord = Some((lat, lon));
                            coord_is_title = is_title;
                        }
                    }
                }
            }
            "featured article" | "featured list" => out.flags |= FLAG_FEATURED,
            "good article" | "ga icon" => out.flags |= FLAG_GOOD,
            "disambiguation" | "disambig" | "disamb" | "dab" | "dmbox" | "hndis" | "geodis" => {
                out.flags |= FLAG_DISAMBIG;
            }
            _ => {
                // "airport disambiguation", "school disambiguation", ...
                if name.ends_with("disambiguation") {
                    out.flags |= FLAG_DISAMBIG;
                } else if out.kind.is_none() {
                    if let Some(k) = name.strip_prefix("infobox") {
                        let k = k.trim();
                        if !k.is_empty() {
                            out.kind = Some(truncate(k, MAX_KIND));
                        }
                    }
                }
            }
        }
    }
    out
}

/// First template argument: after '|', up to the next '|' or '}}'. Good
/// enough for {{Short description}}, whose argument is plain prose; a
/// nested template in position one yields garbage that the length cap
/// contains and nobody stores.
fn first_arg(body: &str) -> Option<&str> {
    let after = body.strip_prefix('|')?;
    let end = after.find(['|', '}']).unwrap_or(after.len());
    Some(after[..end].trim())
}

/// Parse a {{coord}} body: decimal ("|40.71|-74.0") or DMS
/// ("|40|42|46|N|74|0|22|W"), with trailing named parameters. Returns
/// (lat, lon, had display=title).
fn parse_coord(body: &str) -> Option<(f32, f32, bool)> {
    let end = body.find("}}").unwrap_or(body.len());
    let body = &body[..end];
    let is_title = body.contains("display=title")
        || body.contains("display=t|")
        || body.ends_with("display=t");

    let mut nums: Vec<f64> = Vec::with_capacity(8);
    let mut lat: Option<f64> = None;
    let mut lon: Option<f64> = None;
    for part in body.split('|') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        match p {
            "N" | "S" => {
                let v = dms(&nums)?;
                lat = Some(if p == "S" { -v } else { v });
                nums.clear();
            }
            "E" | "W" => {
                let v = dms(&nums)?;
                lon = Some(if p == "W" { -v } else { v });
                nums.clear();
            }
            _ => {
                if let Ok(v) = p.parse::<f64>() {
                    nums.push(v);
                } else {
                    break; // named params begin; positional part is over
                }
            }
        }
    }
    if lat.is_none() && lon.is_none() && nums.len() >= 2 {
        // Decimal form: exactly lat, lon.
        lat = Some(nums[0]);
        lon = Some(nums[1]);
    }
    let (lat, lon) = (lat?, lon?);
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    Some((lat as f32, lon as f32, is_title))
}

fn dms(nums: &[f64]) -> Option<f64> {
    match nums {
        [d] => Some(*d),
        [d, m] => Some(d + m / 60.0),
        [d, m, s] => Some(d + m / 60.0 + s / 3600.0),
        _ => None,
    }
}

/// Truncate on a char boundary; a cut description beats a lost one.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_description_and_none() {
        let e = extract(
            "{{Short description|German-born theoretical physicist}}\ntext",
            "A",
        );
        assert_eq!(
            e.description.as_deref(),
            Some("German-born theoretical physicist")
        );
        assert_eq!(extract("{{short description|none}}", "A").description, None);
        // First one wins; later ones are noise.
        let e = extract(
            "{{Short description|First}} {{Short description|Second}}",
            "A",
        );
        assert_eq!(e.description.as_deref(), Some("First"));
    }

    #[test]
    fn disambiguation_by_template_suffix_and_title() {
        assert_eq!(
            extract("{{Disambiguation}}", "Mercury").flags,
            FLAG_DISAMBIG
        );
        assert_eq!(
            extract("{{hndis|name=Smith, John}}", "John Smith").flags,
            FLAG_DISAMBIG
        );
        assert_eq!(
            extract("{{Airport disambiguation}}", "X").flags,
            FLAG_DISAMBIG
        );
        assert_eq!(
            extract("body only", "Mercury (disambiguation)").flags,
            FLAG_DISAMBIG
        );
        assert_eq!(extract("{{Infobox person}}", "Jane Doe").flags, 0);
    }

    #[test]
    fn infobox_kind_takes_the_first() {
        let e = extract("{{Infobox film\n|name=X}} {{Infobox person}}", "X");
        assert_eq!(e.kind.as_deref(), Some("film"));
        assert_eq!(
            extract("{{infobox}}", "X").kind,
            None,
            "bare infobox has no kind"
        );
    }

    #[test]
    fn quality_flags() {
        let e = extract("{{Featured article}}", "A");
        assert_eq!(e.flags, FLAG_FEATURED);
        assert_eq!(extract("{{good article}}", "A").flags, FLAG_GOOD);
    }

    #[test]
    fn coord_decimal_dms_and_title_preference() {
        let e = extract("{{coord|40.7127|-74.0059|display=inline}}", "NYC");
        let (lat, lon) = e.coord.unwrap();
        assert!((lat - 40.7127).abs() < 1e-4 && (lon + 74.0059).abs() < 1e-4);

        let e = extract("{{coord|40|42|46|N|74|0|22|W|display=title}}", "NYC");
        let (lat, lon) = e.coord.unwrap();
        assert!((lat - 40.7128).abs() < 1e-3, "{lat}");
        assert!((lon + 74.0061).abs() < 1e-3, "{lon}");

        // A display=title coord replaces an earlier incidental one.
        let e = extract(
            "{{coord|10|20|display=inline}} later {{coord|30|40|display=title}}",
            "X",
        );
        assert_eq!(e.coord, Some((30.0, 40.0)));
        // ...but an incidental one never replaces the title coord.
        let e = extract("{{coord|30|40|display=title}} later {{coord|10|20}}", "X");
        assert_eq!(e.coord, Some((30.0, 40.0)));

        // Out-of-range coordinates are lies, not data.
        assert_eq!(extract("{{coord|999|10}}", "X").coord, None);
    }

    #[test]
    fn empty_and_hostile_input() {
        assert_eq!(extract("", "A"), Extras::default());
        assert_eq!(extract("{{", "A").flags, 0);
        assert_eq!(extract("{{}}{{|}}", "A"), Extras::default());
        // A 100 KB name does not blow the scanner up.
        let long = format!("{{{{{}|x}}}}", "n".repeat(100_000));
        assert_eq!(extract(&long, "A"), Extras::default());
    }

    /// The enwiki crash, preserved: a multibyte char straddling byte 64 of a
    /// template name panicked the old byte-index clamp. Any prefix length of
    /// multibyte-heavy names must be safe.
    #[test]
    fn multibyte_never_panics_at_any_boundary() {
        for pad in 0..70 {
            let name = format!("{}–dash", "x".repeat(pad));
            let text = format!("{{{{{name}|arg}}}} tail");
            let _ = extract(&text, "A"); // must not panic, whatever it finds
        }
        // And fully multibyte names, straddling everywhere.
        let geo = "სამხედრო".repeat(12);
        let _ = extract(&format!("{{{{{geo}|x}}}}"), "A");
        let _ = extract("{{coord|„40“|20}}", "A");
    }
}
