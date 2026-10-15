//! Content identity shared by puzzle links, pools and runtime state.
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, path::Path};

pub fn fingerprint(dir: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    for name in ["titles.parquet", "edges.parquet"] {
        let mut file = File::open(dir.join(name))?;
        hash.update(name.as_bytes());
        hash.update(file.metadata()?.len().to_le_bytes());
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hash.update(&buf[..n]);
        }
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_size_replacement_changes_identity() {
        let dir = std::env::temp_dir().join(format!("wp-dataset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("titles.parquet"), b"titles").unwrap();
        std::fs::write(dir.join("edges.parquet"), b"edges-a").unwrap();
        let first = fingerprint(&dir).unwrap();
        assert_eq!(first, fingerprint(&dir).unwrap());
        std::fs::write(dir.join("edges.parquet"), b"edges-b").unwrap();
        assert_ne!(first, fingerprint(&dir).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
