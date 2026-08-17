//! Stream real dump pages through the v3 extractor, looking for panics.
//!
//! The extractor's only failure mode that matters is a panic mid-parse —
//! it costs an hour of PC time per discovery. Unit tests cover the cases we
//! thought of; this covers the ones we did not, by running every page of a
//! real dump through it. Run before shipping extractor changes:
//!
//!   cargo run --release --example extras_torture -- data/dumps/enwiki-...bz2 2000000

use std::io::BufReader;
use std::process::{Command, Stdio};

use wiki_parser::dump::stream_pages;
use wiki_parser::extras::extract;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let dump = args.next().expect("usage: extras_torture <dump.bz2> [max_pages]");
    let max: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(u64::MAX);

    let mut child = Command::new("lbzip2")
        .args(["-dc", &dump])
        .stdout(Stdio::piped())
        .spawn()?;
    let reader = BufReader::with_capacity(8 << 20, child.stdout.take().unwrap());

    let mut n = 0u64;
    let mut found = (0u64, 0u64, 0u64, 0u64); // descs, kinds, flags, coords
    let res = stream_pages(reader, |p| {
        let e = extract(&p.text, &p.title);
        found.0 += e.description.is_some() as u64;
        found.1 += e.kind.is_some() as u64;
        found.2 += (e.flags != 0) as u64;
        found.3 += e.coord.is_some() as u64;
        n += 1;
        if n % 250_000 == 0 {
            eprintln!("  {n} pages tortured…");
        }
        if n >= max {
            anyhow::bail!("__done__");
        }
        Ok(())
    });
    match res {
        Err(e) if e.to_string().contains("__done__") => {}
        Err(e) => return Err(e),
        Ok(_) => {}
    }
    let _ = child.kill();
    eprintln!(
        "clean: {n} pages, no panics. descs {}, kinds {}, flags {}, coords {}",
        found.0, found.1, found.2, found.3
    );
    Ok(())
}
