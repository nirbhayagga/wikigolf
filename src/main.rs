use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;
use quick_xml::events::Event;
use quick_xml::Reader;
use regex::Regex;
use lazy_static::lazy_static;
use std::io;
use std::io::Write;

lazy_static! {
    static ref LINK_RE: Regex = Regex::new(r"\[\[([^|\]]+)(?:\|[^\]]+)?\]\]").unwrap();
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let stdin = io::stdin();
    let mut reader = Reader::from_reader(stdin.lock());
    
    // API fix: call trim_text directly on the reader
    reader.trim_text(true);

    let mut buf = Vec::new();
    let mut current_title = String::new();
    let mut in_title = false;
    let mut in_text = false;
    let mut text_buffer = String::new();
    let mut article_count: u64 = 0;

    println!("Source,Target");

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                match e.name().as_ref() {
                    b"title" => in_title = true,
                    b"text" => in_text = true,
                    _ => (),
                }
            }
            Ok(Event::Text(e)) => {
                if in_title {
                    current_title = e.unescape()?.into_owned();
                } else if in_text {
                    text_buffer.push_str(&e.unescape()?);
                }
            }
            Ok(Event::End(ref e)) => {
                match e.name().as_ref() {
                    b"title" => in_title = false,
                    b"text" => {
                        in_text = false;
                        process_article(&current_title, &text_buffer);
                        text_buffer.clear();
                    }
                    b"page" => {
                        article_count += 1;
                        if article_count % 100_000 == 0 {
                            eprint!("\r   Parsed {:>10} articles", article_count);
                            io::stderr().flush().ok();
                        }
                        current_title.clear();
                    }
                    _ => (),
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => continue,
            _ => (),
        }
        buf.clear();
    }
    eprintln!("\r   Parsed {:>10} articles (done)", article_count);
    Ok(())
}

fn process_article(title: &str, wikitext: &str) {
    if title.contains(':') || title.starts_with("Main Page") {
        return;
    }

    // Chain splits correctly — each stage truncates from the previous result
    let content = wikitext
        .split("== See also ==").next().unwrap_or(wikitext);
    let content = content
        .split("== References ==").next().unwrap_or(content);
    let content = content
        .split("== External links ==").next().unwrap_or(content);

    for cap in LINK_RE.captures_iter(content) {
        let target = &cap[1].trim();
        if !target.is_empty() && !target.contains(':') && !target.starts_with('#') {
            // Sanitize for RFC 4180 CSV: escape double-quotes and strip
            // newlines that would break row boundaries in the output
            let clean_source = title.replace('"', "\"\"").replace('\n', " ").replace('\r', "");
            let clean_target = target.replace('"', "\"\"").replace('\n', " ").replace('\r', "");
            println!("\"{}\",\"{}\"", clean_source, clean_target);
        }
    }
}

