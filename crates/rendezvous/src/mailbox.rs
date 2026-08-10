//! Nameplate mailbox: the in-memory store pairing numeric nameplates (1-9999)
//! with opaque ticket payloads.
//!
//! SECURITY (F1): the server stores and routes ONLY by numeric nameplate.
//! The word-code password never reaches this module — `Nameplate::parse`
//! accepts nothing but canonical ASCII digits in `1..=9999`, so a word-bearing
//! request path is rejected before any lookup happens.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::time::{Duration, Instant};

use rand::Rng;

pub const NAMEPLATE_MIN: u32 = 1;
pub const NAMEPLATE_MAX: u32 = 9999;
pub const MAX_TICKET_LENGTH: usize = 4096;

/// A server-allocated numeric nameplate. Invariant: value is in 1..=9999.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Nameplate(u32);

impl Nameplate {
    /// Parse a nameplate from a raw path segment.
    ///
    /// Accepts only canonical ASCII decimal form: `[1-9][0-9]*` with value in
    /// 1..=9999. Rejects empty strings, any non-digit character (including the
    /// pairing words and the `-` separator), leading zeros, `+5`, `1_000`, and
    /// out-of-range values.
    pub fn parse(raw: &str) -> Result<Self, NameplateError> {
        if raw.is_empty() {
            return Err(NameplateError::Empty);
        }
        if !raw.bytes().all(|b| b.is_ascii_digit()) {
            return Err(NameplateError::NotNumeric);
        }
        if raw.len() > 1 && raw.starts_with('0') {
            return Err(NameplateError::LeadingZero);
        }
        let value: u32 = raw.parse().map_err(|_| NameplateError::NotNumeric)?;
        if !(NAMEPLATE_MIN..=NAMEPLATE_MAX).contains(&value) {
            return Err(NameplateError::OutOfRange);
        }
        Ok(Self(value))
    }

    pub fn value(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for Nameplate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Why a nameplate path segment was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameplateError {
    Empty,
    NotNumeric,
    LeadingZero,
    OutOfRange,
}

impl std::fmt::Display for NameplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "nameplate is empty"),
            Self::NotNumeric => write!(f, "nameplate must be numeric digits only"),
            Self::LeadingZero => write!(f, "nameplate must not have leading zeros"),
            Self::OutOfRange => write!(f, "nameplate must be between 1 and 9999"),
        }
    }
}

impl std::error::Error for NameplateError {}

/// The observable lifecycle of a nameplate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairState {
    Pending,
    Claimed,
    Expired,
}

impl PairState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Claimed => "claimed",
            Self::Expired => "expired",
        }
    }
}

/// Why a claim failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimError {
    NotFound,
    Expired,
}

#[derive(Debug)]
struct Entry {
    ticket: String,
    expires_at: Instant,
    claimed: bool,
}

/// In-memory nameplate -> ticket mailbox. Single-node MVP: no DB.
#[derive(Debug, Default)]
pub struct Mailbox {
    entries: HashMap<Nameplate, Entry>,
}

impl Mailbox {
    /// Allocate an unused random nameplate and store the ticket under it.
    ///
    /// The ticket is opaque: stored verbatim, never inspected. Returns the
    /// allocated nameplate.
    pub fn allocate(&mut self, ticket: String, ttl: Duration) -> Nameplate {
        let mut rng = rand::rng();
        let nameplate = loop {
            let candidate = Nameplate(rng.random_range(NAMEPLATE_MIN..=NAMEPLATE_MAX));
            if !self.entries.contains_key(&candidate) {
                break candidate;
            }
        };
        self.entries.insert(
            nameplate,
            Entry {
                ticket,
                expires_at: Instant::now() + ttl,
                claimed: false,
            },
        );
        nameplate
    }

    /// One-shot claim: return the ticket and mark the entry claimed. A second
    /// claim on the same nameplate returns [`ClaimError::NotFound`].
    pub fn claim(&mut self, nameplate: Nameplate, now: Instant) -> Result<String, ClaimError> {
        let entry = self
            .entries
            .get_mut(&nameplate)
            .ok_or(ClaimError::NotFound)?;
        if now >= entry.expires_at {
            return Err(ClaimError::Expired);
        }
        if entry.claimed {
            return Err(ClaimError::NotFound);
        }
        entry.claimed = true;
        Ok(entry.ticket.clone())
    }

    /// Current state of a nameplate, or `None` if it does not exist.
    pub fn status(&self, nameplate: Nameplate, now: Instant) -> Option<PairState> {
        let entry = self.entries.get(&nameplate)?;
        Some(if now >= entry.expires_at {
            PairState::Expired
        } else if entry.claimed {
            PairState::Claimed
        } else {
            PairState::Pending
        })
    }

    /// Remove all expired entries (called by the cleanup task). Returns how
    /// many were removed.
    pub fn purge_expired(&mut self, now: Instant) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, entry| entry.expires_at > now);
        before - self.entries.len()
    }
}

/// Sliding-window per-IP rate limiter (drift server pattern).
#[derive(Debug, Default)]
pub struct RateLimiter {
    entries: HashMap<IpAddr, VecDeque<Instant>>,
}

impl RateLimiter {
    /// Record one access for `ip` under a 60-second sliding window.
    /// Returns `true` if under `limit`; `false` if the limit is exceeded (the
    /// rejected access is NOT recorded).
    pub fn check(&mut self, ip: IpAddr, limit: usize) -> bool {
        let now = Instant::now();
        let window = self.entries.entry(ip).or_default();
        while let Some(front) = window.front() {
            if now.duration_since(*front) >= Duration::from_secs(60) {
                window.pop_front();
            } else {
                break;
            }
        }

        if window.len() >= limit {
            return false;
        }

        window.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nameplate_parse_accepts_canonical_numeric() {
        for raw in ["1", "7", "42", "999", "9999"] {
            let nameplate = Nameplate::parse(raw).unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert_eq!(nameplate.value(), raw.parse::<u32>().unwrap());
        }
    }

    #[test]
    fn nameplate_parse_rejects_words_and_non_numeric() {
        // SECURITY F1: the word password and its "-" separator must never pass.
        for raw in [
            "",
            "abc",
            "7-correct-horse-battery",
            "1a",
            "12b3",
            "+5",
            "-7",
            "1_000",
        ] {
            assert!(
                Nameplate::parse(raw).is_err(),
                "expected rejection for {raw:?}"
            );
        }
    }

    #[test]
    fn nameplate_parse_rejects_out_of_range_and_leading_zeros() {
        assert_eq!(Nameplate::parse("0"), Err(NameplateError::OutOfRange));
        assert_eq!(Nameplate::parse("10000"), Err(NameplateError::OutOfRange));
        assert_eq!(Nameplate::parse("007"), Err(NameplateError::LeadingZero));
        // "99999" overflows digits but parses fine as u32 — still out of range.
        assert_eq!(Nameplate::parse("99999"), Err(NameplateError::OutOfRange));
        // Leading-zero with single digit "0" is a range error, not leading zero.
        assert_eq!(Nameplate::parse("0"), Err(NameplateError::OutOfRange));
    }

    #[test]
    fn allocate_returns_unused_nameplate_in_range() {
        let mut mailbox = Mailbox::default();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..100 {
            let nameplate = mailbox.allocate("t".to_owned(), Duration::from_secs(600));
            assert!((NAMEPLATE_MIN..=NAMEPLATE_MAX).contains(&nameplate.value()));
            assert!(seen.insert(nameplate), "duplicate allocation");
        }
    }

    #[test]
    fn claim_is_one_shot() {
        let mut mailbox = Mailbox::default();
        let nameplate = mailbox.allocate("ticket-1".to_owned(), Duration::from_secs(600));
        let now = Instant::now();

        assert_eq!(mailbox.claim(nameplate, now), Ok("ticket-1".to_owned()));
        // Second claim on the same nameplate is treated as not found.
        assert_eq!(mailbox.claim(nameplate, now), Err(ClaimError::NotFound));
    }

    #[test]
    fn expired_entry_is_not_claimable_and_reports_expired() {
        let mut mailbox = Mailbox::default();
        // TTL zero: the entry expires immediately.
        let nameplate = mailbox.allocate("t".to_owned(), Duration::ZERO);
        let later = Instant::now() + Duration::from_millis(1);

        assert_eq!(mailbox.claim(nameplate, later), Err(ClaimError::Expired));
        assert_eq!(mailbox.status(nameplate, later), Some(PairState::Expired));
    }

    #[test]
    fn purge_expired_removes_only_stale_entries() {
        let mut mailbox = Mailbox::default();
        let expired = mailbox.allocate("old".to_owned(), Duration::ZERO);
        let fresh = mailbox.allocate("new".to_owned(), Duration::from_secs(600));
        let now = Instant::now();

        assert_eq!(mailbox.purge_expired(now), 1);
        assert_eq!(mailbox.status(expired, now), None);
        assert_eq!(mailbox.status(fresh, now), Some(PairState::Pending));
    }

    #[test]
    fn rate_limiter_enforces_limit_per_ip() {
        let mut limiter = RateLimiter::default();
        let ip = IpAddr::from([127, 0, 0, 1]);

        for _ in 0..10 {
            assert!(limiter.check(ip, 10));
        }
        assert!(!limiter.check(ip, 10));
        // Rejected access is not recorded: still exactly at the limit.
        assert!(!limiter.check(ip, 10));

        // Different IP has its own independent window.
        let other = IpAddr::from([127, 0, 0, 2]);
        assert!(limiter.check(other, 10));
    }
}
