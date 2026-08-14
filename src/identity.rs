//! Signed anonymous player identity.
//!
//! A leaderboard needs to know that two submissions came from the same person,
//! or one player takes the whole board under twenty nicknames. It does not need
//! to know *who* that person is — so there are no accounts, no email, no
//! password resets and no personal data: just an opaque random id the server
//! signs so it cannot be forged or swapped for someone else's.
//!
//! Cookie value is `<id>.<mac>`, where mac is HMAC-SHA256(secret, id)
//! truncated to 16 bytes. Without the secret a client can mint an id but not a
//! valid signature, so it cannot impersonate an existing player — the point is
//! not that identities are scarce (clearing cookies makes a new one) but that
//! they are unforgeable and stable.

use anyhow::{Context, Result};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

type HmacSha256 = Hmac<Sha256>;

const COOKIE: &str = "wr_id";
const SECRET_FILE: &str = ".wiki-race-secret";
const ID_BYTES: usize = 16;
const MAC_BYTES: usize = 16;

pub struct Identity {
    secret: Vec<u8>,
}

fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut f = fs::File::open("/dev/urandom").context("opening /dev/urandom")?;
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Identity {
    /// Load the signing secret, creating it on first run.
    ///
    /// Persisting it matters: a per-start secret would invalidate every
    /// player's identity on each deploy, silently resetting the leaderboard's
    /// notion of who is who.
    pub fn load_or_create(dir: &Path) -> Result<Identity> {
        let path: PathBuf = dir.join(SECRET_FILE);
        if let Ok(s) = fs::read(&path) {
            if s.len() >= 32 {
                return Ok(Identity { secret: s });
            }
        }
        let secret = random_bytes(32)?;
        fs::write(&path, &secret)
            .with_context(|| format!("writing {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // The secret is what makes signatures unforgeable; do not leave it
            // world-readable next to the data files.
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
        Ok(Identity { secret })
    }

    fn sign(&self, id: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.secret).expect("hmac accepts any key size");
        mac.update(id.as_bytes());
        hex(&mac.finalize().into_bytes()[..MAC_BYTES])
    }

    /// Mint a fresh signed identity, returned as the cookie value.
    pub fn issue(&self) -> Result<String> {
        let id = hex(&random_bytes(ID_BYTES)?);
        let mac = self.sign(&id);
        Ok(format!("{id}.{mac}"))
    }

    /// Recover the player id from a cookie value, if the signature holds.
    pub fn verify(&self, value: &str) -> Option<String> {
        let (id, mac) = value.split_once('.')?;
        if id.len() != ID_BYTES * 2 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let expect = self.sign(id);
        // Constant-time compare: a byte-at-a-time result leaks how much of a
        // guessed signature was right, which is enough to forge one.
        if ct_eq(expect.as_bytes(), mac.as_bytes()) {
            Some(id.to_string())
        } else {
            None
        }
    }

    /// The `Set-Cookie` header value for a freshly issued identity.
    pub fn cookie_header(value: &str, secure: bool) -> String {
        let mut s = format!(
            "{COOKIE}={value}; Path=/; Max-Age=31536000; HttpOnly; SameSite=Lax"
        );
        if secure {
            s.push_str("; Secure");
        }
        s
    }

    /// Pull our cookie out of a `Cookie` header.
    pub fn from_header(header: &str) -> Option<&str> {
        header.split(';').find_map(|part| {
            let (k, v) = part.trim().split_once('=')?;
            (k == COOKIE).then_some(v)
        })
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident() -> Identity {
        Identity { secret: vec![7u8; 32] }
    }

    #[test]
    fn issued_cookies_verify() {
        let i = ident();
        let c = i.issue().unwrap();
        let id = i.verify(&c).expect("freshly issued cookie must verify");
        assert_eq!(id.len(), ID_BYTES * 2);
        // Stable: the same cookie always names the same player.
        assert_eq!(i.verify(&c).unwrap(), id);
    }

    #[test]
    fn tampered_id_is_rejected() {
        let i = ident();
        let c = i.issue().unwrap();
        let (id, mac) = c.split_once('.').unwrap();
        let mut bad: Vec<char> = id.chars().collect();
        bad[0] = if bad[0] == 'a' { 'b' } else { 'a' };
        let forged = format!("{}.{}", bad.into_iter().collect::<String>(), mac);
        assert_eq!(i.verify(&forged), None, "id must not be swappable");
    }

    #[test]
    fn unsigned_and_malformed_are_rejected() {
        let i = ident();
        assert_eq!(i.verify("deadbeef"), None);
        assert_eq!(i.verify(&format!("{}.{}", "a".repeat(32), "0".repeat(32))), None);
        assert_eq!(i.verify("zz.00"), None);
        assert_eq!(i.verify(""), None);
    }

    #[test]
    fn a_different_secret_cannot_sign_for_us() {
        let a = ident();
        let b = Identity { secret: vec![9u8; 32] };
        let c = b.issue().unwrap();
        assert_eq!(a.verify(&c), None);
    }

    #[test]
    fn cookie_is_parsed_out_of_a_crowded_header() {
        assert_eq!(Identity::from_header("foo=1; wr_id=abc.def; bar=2"), Some("abc.def"));
        assert_eq!(Identity::from_header("wr_id=x"), Some("x"));
        assert_eq!(Identity::from_header("other=1"), None);
    }

    #[test]
    fn cookie_header_flags() {
        let h = Identity::cookie_header("v", false);
        assert!(h.contains("HttpOnly") && h.contains("SameSite=Lax"));
        assert!(!h.contains("Secure"));
        assert!(Identity::cookie_header("v", true).contains("; Secure"));
    }
}
