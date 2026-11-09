//! Contact strings (`wzp1…`) and the persisted [`ContactBook`].
//!
//! # Contact string format (version 1)
//!
//! `wzp1` + lower-case RFC 4648 base32 (no padding) of:
//!
//! ```text
//! u8      version (= 1)
//! [32]    endpoint id (= WZP Ed25519 identity public key)
//! u8 len  display name, UTF-8, <= 64 bytes
//! u8 len  relay URL, UTF-8, <= 200 bytes (0 = none)
//! u8 n    direct addresses (<= 8), each: u8 family (4|6), 4|16 ip bytes, u16 BE port
//! [4]     checksum = SHA-256("wzp-contact-v1" || all previous bytes)[..4]
//! ```
//!
//! Parsing is case-insensitive, so [`ContactCard::to_qr_string`] emits the
//! upper-case form which fits the QR "alphanumeric" mode (denser codes).

use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use data_encoding::BASE32_NOPAD;
use iroh::{EndpointAddr, EndpointId, RelayUrl};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    error::{Error, Result},
    identity,
};

/// Prefix of every contact string.
pub const CONTACT_PREFIX: &str = "wzp1";
const VERSION: u8 = 1;
const CHECKSUM_DOMAIN: &[u8] = b"wzp-contact-v1";
const MAX_NAME: usize = 64;
const MAX_RELAY: usize = 200;
const MAX_ADDRS: usize = 8;

/// The decoded content of a contact string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactCard {
    pub id: EndpointId,
    pub name: String,
    pub relay_url: Option<RelayUrl>,
    pub direct_addrs: Vec<SocketAddr>,
}

impl ContactCard {
    /// Encode as `wzp1…` (lower case).
    pub fn encode(&self) -> String {
        let mut b = Vec::with_capacity(128);
        b.push(VERSION);
        b.extend_from_slice(self.id.as_bytes());
        let name = truncate_utf8(&self.name, MAX_NAME);
        b.push(name.len() as u8);
        b.extend_from_slice(name.as_bytes());
        let relay = self
            .relay_url
            .as_ref()
            .map(|u| u.to_string())
            .filter(|s| s.len() <= MAX_RELAY)
            .unwrap_or_default();
        b.push(relay.len() as u8);
        b.extend_from_slice(relay.as_bytes());
        let addrs: Vec<_> = self.direct_addrs.iter().take(MAX_ADDRS).collect();
        b.push(addrs.len() as u8);
        for a in addrs {
            match a.ip() {
                IpAddr::V4(ip) => {
                    b.push(4);
                    b.extend_from_slice(&ip.octets());
                }
                IpAddr::V6(ip) => {
                    b.push(6);
                    b.extend_from_slice(&ip.octets());
                }
            }
            b.extend_from_slice(&a.port().to_be_bytes());
        }
        let ck = checksum(&b);
        b.extend_from_slice(&ck);
        format!(
            "{CONTACT_PREFIX}{}",
            BASE32_NOPAD.encode(&b).to_ascii_lowercase()
        )
    }

    /// Upper-case form for QR codes (alphanumeric mode).
    pub fn to_qr_string(&self) -> String {
        self.encode().to_ascii_uppercase()
    }

    /// Parse and validate a contact string (case-insensitive, surrounding
    /// whitespace ignored).
    pub fn parse(s: &str) -> Result<Self> {
        let bad = |m: &str| Error::InvalidContact(m.to_string());
        let s = s.trim().to_ascii_uppercase();
        let body = s
            .strip_prefix(&CONTACT_PREFIX.to_ascii_uppercase())
            .ok_or_else(|| bad("missing wzp1 prefix"))?;
        let raw = BASE32_NOPAD
            .decode(body.as_bytes())
            .map_err(|_| bad("not base32"))?;
        if raw.len() < 1 + 32 + 3 + 4 {
            return Err(bad("too short"));
        }
        let (payload, ck) = raw.split_at(raw.len() - 4);
        if checksum(payload) != ck {
            return Err(bad("checksum mismatch (typo?)"));
        }
        let mut r = Reader(payload);
        if r.u8()? != VERSION {
            return Err(bad("unsupported version"));
        }
        let id_bytes: [u8; 32] = r.take(32)?.try_into().unwrap();
        let id = EndpointId::from_bytes(&id_bytes).map_err(|_| bad("invalid public key"))?;
        let n = r.u8()? as usize;
        if n > MAX_NAME {
            return Err(bad("name too long"));
        }
        let name = String::from_utf8(r.take(n)?.to_vec()).map_err(|_| bad("name not UTF-8"))?;
        let n = r.u8()? as usize;
        let relay_url = if n == 0 {
            None
        } else {
            let s = std::str::from_utf8(r.take(n)?).map_err(|_| bad("relay not UTF-8"))?;
            Some(s.parse().map_err(|_| bad("invalid relay URL"))?)
        };
        let n = r.u8()? as usize;
        if n > MAX_ADDRS {
            return Err(bad("too many addresses"));
        }
        let mut direct_addrs = Vec::with_capacity(n);
        for _ in 0..n {
            let ip = match r.u8()? {
                4 => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(r.take(4)?).unwrap())),
                6 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(r.take(16)?).unwrap())),
                _ => return Err(bad("bad address family")),
            };
            let port = u16::from_be_bytes(r.take(2)?.try_into().unwrap());
            direct_addrs.push(SocketAddr::new(ip, port));
        }
        if !r.0.is_empty() {
            return Err(bad("trailing bytes"));
        }
        Ok(Self {
            id,
            name,
            relay_url,
            direct_addrs,
        })
    }

    /// iroh dialing info carried by this card.
    pub fn endpoint_addr(&self) -> EndpointAddr {
        let mut a = EndpointAddr::new(self.id);
        if let Some(u) = &self.relay_url {
            a = a.with_relay_url(u.clone());
        }
        for d in &self.direct_addrs {
            a = a.with_ip_addr(*d);
        }
        a
    }
}

fn checksum(b: &[u8]) -> [u8; 4] {
    let mut h = Sha256::new();
    h.update(CHECKSUM_DOMAIN);
    h.update(b);
    let d = h.finalize();
    [d[0], d[1], d[2], d[3]]
}

fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(Error::InvalidContact("truncated".into()));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
}

/// How much we trust a contact's key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    /// Pinned on first use; fingerprint not yet compared out of band.
    Tofu,
    /// The user compared the fingerprint out of band.
    Verified,
}

/// A contact in the [`ContactBook`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contact {
    /// iroh endpoint id == WZP identity public key.
    pub id: EndpointId,
    pub name: String,
    /// WZP fingerprint (`xxxx:…`, 8 groups) for out-of-band verification.
    pub fingerprint: String,
    pub relay_url: Option<RelayUrl>,
    #[serde(default)]
    pub direct_addrs: Vec<SocketAddr>,
    pub trust: Trust,
    /// Unix seconds.
    pub added_at: u64,
    /// Unix seconds of the last successful exchange with this contact.
    #[serde(default)]
    pub last_seen: Option<u64>,
}

impl Contact {
    /// Build a fresh TOFU contact from a card.
    pub fn from_card(card: &ContactCard) -> Self {
        Self {
            id: card.id,
            name: card.name.clone(),
            fingerprint: identity::fingerprint(&card.id),
            relay_url: card.relay_url.clone(),
            direct_addrs: card.direct_addrs.clone(),
            trust: Trust::Tofu,
            added_at: now_secs(),
            last_seen: None,
        }
    }

    /// iroh dialing info for this contact.
    pub fn endpoint_addr(&self) -> EndpointAddr {
        ContactCard {
            id: self.id,
            name: String::new(),
            relay_url: self.relay_url.clone(),
            direct_addrs: self.direct_addrs.clone(),
        }
        .endpoint_addr()
    }
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct BookFile {
    version: u32,
    contacts: Vec<Contact>,
}

/// Contacts persisted as `contacts.json` in a directory.
#[derive(Debug)]
pub struct ContactBook {
    path: Option<PathBuf>,
    inner: Mutex<BTreeMap<EndpointId, Contact>>,
}

impl ContactBook {
    /// File name used inside the data directory.
    pub const FILE_NAME: &'static str = "contacts.json";

    /// Open (or create empty) the book in `dir`.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(Self::FILE_NAME);
        let mut map = BTreeMap::new();
        if path.exists() {
            let data = std::fs::read(&path)?;
            let file: BookFile = serde_json::from_slice(&data)?;
            for c in file.contacts {
                map.insert(c.id, c);
            }
        }
        Ok(Self {
            path: Some(path),
            inner: Mutex::new(map),
        })
    }

    /// A book that is never written to disk.
    pub fn in_memory() -> Self {
        Self {
            path: None,
            inner: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn get(&self, id: &EndpointId) -> Option<Contact> {
        self.inner.lock().unwrap().get(id).cloned()
    }

    pub fn contains(&self, id: &EndpointId) -> bool {
        self.inner.lock().unwrap().contains_key(id)
    }

    pub fn list(&self) -> Vec<Contact> {
        self.inner.lock().unwrap().values().cloned().collect()
    }

    /// Insert or update. An existing entry keeps its trust level and
    /// `added_at`; name and addresses are refreshed. Returns the stored contact.
    pub fn upsert(&self, c: Contact) -> Result<Contact> {
        let stored = {
            let mut m = self.inner.lock().unwrap();
            let entry = m
                .entry(c.id)
                .and_modify(|e| {
                    if !c.name.is_empty() {
                        e.name = c.name.clone();
                    }
                    if c.relay_url.is_some() {
                        e.relay_url = c.relay_url.clone();
                    }
                    if !c.direct_addrs.is_empty() {
                        e.direct_addrs = c.direct_addrs.clone();
                    }
                })
                .or_insert(c);
            entry.clone()
        };
        self.save()?;
        Ok(stored)
    }

    pub fn remove(&self, id: &EndpointId) -> Result<Option<Contact>> {
        let r = self.inner.lock().unwrap().remove(id);
        self.save()?;
        Ok(r)
    }

    /// Set trust level; returns false if unknown.
    pub fn set_trust(&self, id: &EndpointId, trust: Trust) -> Result<bool> {
        let found = match self.inner.lock().unwrap().get_mut(id) {
            Some(c) => {
                c.trust = trust;
                true
            }
            None => false,
        };
        if found {
            self.save()?;
        }
        Ok(found)
    }

    /// Record `last_seen` in memory (persisted on the next save).
    pub(crate) fn touch(&self, id: &EndpointId, ts: u64) {
        if let Some(c) = self.inner.lock().unwrap().get_mut(id) {
            c.last_seen = Some(ts);
        }
    }

    /// Write the book atomically (temp file + rename).
    pub fn save(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let file = BookFile {
            version: 1,
            contacts: self.list(),
        };
        let data = serde_json::to_vec_pretty(&file)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::secret_key_from_seed;

    fn card() -> ContactCard {
        ContactCard {
            id: secret_key_from_seed(&[3u8; 32]).public(),
            name: "Ålice".into(),
            relay_url: Some("https://relay.example.org./".parse().unwrap()),
            direct_addrs: vec![
                "127.0.0.1:4433".parse().unwrap(),
                "[::1]:5555".parse().unwrap(),
            ],
        }
    }

    #[test]
    fn roundtrip_and_case() {
        let c = card();
        let s = c.encode();
        assert!(s.starts_with("wzp1"));
        assert_eq!(ContactCard::parse(&s).unwrap(), c);
        assert_eq!(ContactCard::parse(&c.to_qr_string()).unwrap(), c);
        assert_eq!(ContactCard::parse(&format!("  {s}\n")).unwrap(), c);
        let min = ContactCard {
            relay_url: None,
            direct_addrs: vec![],
            name: String::new(),
            ..c
        };
        assert_eq!(ContactCard::parse(&min.encode()).unwrap(), min);
    }

    #[test]
    fn qr_alphabet() {
        let qr = card().to_qr_string();
        assert!(qr.chars().all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit()));
    }

    #[test]
    fn rejects_garbage_and_typos() {
        let s = card().encode();
        assert!(ContactCard::parse("hello").is_err());
        assert!(ContactCard::parse("wzp1").is_err());
        // flip one character
        let mut chars: Vec<char> = s.chars().collect();
        let i = 20;
        chars[i] = if chars[i] == 'a' { 'b' } else { 'a' };
        let typo: String = chars.into_iter().collect();
        assert!(ContactCard::parse(&typo).is_err());
        // truncated
        assert!(ContactCard::parse(&s[..s.len() - 3]).is_err());
    }

    #[test]
    fn book_persists() {
        let dir = tempfile::tempdir().unwrap();
        let c = Contact::from_card(&card());
        {
            let b = ContactBook::open(dir.path()).unwrap();
            b.upsert(c.clone()).unwrap();
            b.set_trust(&c.id, Trust::Verified).unwrap();
        }
        let b = ContactBook::open(dir.path()).unwrap();
        let got = b.get(&c.id).unwrap();
        assert_eq!(got.trust, Trust::Verified);
        assert_eq!(got.fingerprint, c.fingerprint);
        // re-adding keeps trust
        b.upsert(Contact::from_card(&card())).unwrap();
        assert_eq!(b.get(&c.id).unwrap().trust, Trust::Verified);
        b.remove(&c.id).unwrap();
        assert!(ContactBook::open(dir.path()).unwrap().list().is_empty());
    }
}
