//! Word-code format: `nameplate-word-word-word` (magic-wormhole model).
//!
//! The code has two structurally separate parts (SECURITY, Oracle F1):
//! - **nameplate**: numeric 1-9999, server-visible. The rendezvous allocates,
//!   stores and routes by it only.
//! - **words**: 3 distinct PGP-wordlist words, lowercase, hyphen-joined. This
//!   is the SPAKE2 password and MUST NEVER be sent to the rendezvous.
//!
//! `split` is the structural seam: it separates the two parts on the first
//! `-` so callers can send only the nameplate to the server (magic-wormhole
//! model: `code.split("-", 2)[0]`). `validate` additionally checks the full
//! format (nameplate range, word membership, distinctness).

use std::fmt;

use rand::Rng;

use crate::pairing::wordlist::WORDS;

/// Smallest allocatable nameplate (server-side range).
pub const NAMEPLATE_MIN: u32 = 1;
/// Largest allocatable nameplate (server-side range).
pub const NAMEPLATE_MAX: u32 = 9999;
/// Number of secret words in a code.
pub const WORD_COUNT: usize = 3;

/// A parsed word code: a server-visible `nameplate` plus the secret `words`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordCode {
    nameplate: u32,
    words: [String; WORD_COUNT],
}

impl WordCode {
    /// Generate a code for `nameplate` using `rng`-driven distinct words.
    ///
    /// Fails with [`WordCodeError::InvalidNameplate`] when `nameplate` is
    /// outside `1..=9999`. The returned code never repeats a word.
    pub fn generate(nameplate: u32, rng: &mut impl Rng) -> Result<Self, WordCodeError> {
        if !(NAMEPLATE_MIN..=NAMEPLATE_MAX).contains(&nameplate) {
            return Err(WordCodeError::InvalidNameplate(nameplate.to_string()));
        }
        let mut picked: Vec<&str> = Vec::with_capacity(WORD_COUNT);
        while picked.len() < WORD_COUNT {
            let word = WORDS[rng.random_range(0..WORDS.len())];
            if !picked.contains(&word) {
                picked.push(word);
            }
        }
        Ok(Self {
            nameplate,
            words: [
                picked[0].to_string(),
                picked[1].to_string(),
                picked[2].to_string(),
            ],
        })
    }

    /// Structurally split `s` on the first `-` into `(nameplate, words)`.
    ///
    /// This is the security seam: only the nameplate is meant to leave the
    /// client. Only the nameplate part is validated; the words portion is
    /// returned verbatim (full validation is [`WordCode::validate`]).
    pub fn split(s: &str) -> Result<(u32, String), WordCodeError> {
        let Some((nameplate, words)) = s.split_once('-') else {
            return Err(WordCodeError::MissingHyphen);
        };
        if words.is_empty() {
            return Err(WordCodeError::EmptyWords);
        }
        Ok((parse_nameplate(nameplate)?, words.to_string()))
    }

    /// Fully validate `s` into a [`WordCode`].
    ///
    /// Accepts exactly `nameplate-word-word-word`: numeric nameplate in
    /// `1..=9999`, 3 words from the PGP wordlist, lowercase (enforced by
    /// wordlist membership), hyphen-separated, no duplicates.
    pub fn validate(s: &str) -> Result<Self, WordCodeError> {
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() != WORD_COUNT + 1 {
            return Err(WordCodeError::WrongWordCount(parts.len().saturating_sub(1)));
        }
        let nameplate = parse_nameplate(parts[0])?;
        let mut words: [String; WORD_COUNT] = Default::default();
        for (slot, part) in parts[1..].iter().enumerate() {
            let word = *part;
            if !WORDS.contains(&word) {
                return Err(WordCodeError::UnknownWord(word.to_string()));
            }
            if words.iter().any(|w| w == word) {
                return Err(WordCodeError::DuplicateWord(word.to_string()));
            }
            words[slot] = word.to_string();
        }
        Ok(Self { nameplate, words })
    }

    /// The server-visible numeric nameplate.
    pub fn nameplate(&self) -> u32 {
        self.nameplate
    }

    /// The three secret words (SPAKE2 password material).
    pub fn words(&self) -> &[String; WORD_COUNT] {
        &self.words
    }

    /// The SPAKE2 password: the word portion only, hyphen-joined.
    /// MUST never be sent to the rendezvous.
    pub fn password(&self) -> String {
        self.words.join("-")
    }
}

impl fmt::Display for WordCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.nameplate, self.password())
    }
}

/// Parse a nameplate part: non-empty ASCII digits representing `1..=9999`.
///
/// The digit check is required because Rust's integer `FromStr` accepts
/// separators and signs (`1_000`, `+5`) that the rendezvous would treat
/// differently.
fn parse_nameplate(part: &str) -> Result<u32, WordCodeError> {
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return Err(WordCodeError::InvalidNameplate(part.to_string()));
    }
    match part.parse::<u32>() {
        Ok(n) if (NAMEPLATE_MIN..=NAMEPLATE_MAX).contains(&n) => Ok(n),
        _ => Err(WordCodeError::InvalidNameplate(part.to_string())),
    }
}

/// Errors from generating, splitting or validating a word code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WordCodeError {
    /// No `-` separator present in the code.
    MissingHyphen,
    /// Nameplate part is not a number in `1..=9999`.
    InvalidNameplate(String),
    /// Words portion (after the first `-`) is empty.
    EmptyWords,
    /// Code does not contain exactly [`WORD_COUNT`] words.
    WrongWordCount(usize),
    /// A word is not in the PGP wordlist.
    UnknownWord(String),
    /// A word appears more than once in the code.
    DuplicateWord(String),
}

impl fmt::Display for WordCodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHyphen => {
                write!(f, "code must contain a '-' separating nameplate and words")
            }
            Self::InvalidNameplate(n) => write!(f, "nameplate '{n}' must be a number in 1..=9999"),
            Self::EmptyWords => write!(f, "code must contain words after the nameplate"),
            Self::WrongWordCount(n) => {
                write!(f, "code must contain exactly {WORD_COUNT} words, got {n}")
            }
            Self::UnknownWord(w) => write!(f, "word '{w}' is not in the PGP wordlist"),
            Self::DuplicateWord(w) => write!(f, "word '{w}' appears more than once in the code"),
        }
    }
}

impl std::error::Error for WordCodeError {}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    use super::*;

    /// A fully valid code built from real wordlist words.
    const VALID: &str = "7-adroitness-adviser-aftermath";

    fn rng(seed: u64) -> StdRng {
        StdRng::seed_from_u64(seed)
    }

    #[test]
    fn validate_accepts_valid_code() {
        let code = WordCode::validate(VALID).expect("valid code should parse");
        assert_eq!(code.nameplate(), 7);
        assert_eq!(
            code.words(),
            &[
                "adroitness".to_string(),
                "adviser".to_string(),
                "aftermath".to_string()
            ]
        );
        assert_eq!(code.to_string(), VALID);
        assert_eq!(code.password(), "adroitness-adviser-aftermath");
    }

    #[test]
    fn validate_rejects_too_few_words() {
        assert_eq!(
            WordCode::validate("7-adroitness-adviser"),
            Err(WordCodeError::WrongWordCount(2))
        );
    }

    #[test]
    fn validate_rejects_too_many_words() {
        assert_eq!(
            WordCode::validate("7-adroitness-adviser-aftermath-aftermath"),
            Err(WordCodeError::WrongWordCount(4))
        );
    }

    #[test]
    fn validate_rejects_unknown_word() {
        assert_eq!(
            WordCode::validate("7-adroitness-adviser-zzzz"),
            Err(WordCodeError::UnknownWord("zzzz".to_string()))
        );
    }

    #[test]
    fn validate_rejects_uppercase_word() {
        assert_eq!(
            WordCode::validate("7-ADROITNESS-adviser-aftermath"),
            Err(WordCodeError::UnknownWord("ADROITNESS".to_string()))
        );
    }

    #[test]
    fn validate_rejects_bad_nameplate() {
        for bad in [
            "0-adroitness-adviser-aftermath",
            "10000-adroitness-adviser-aftermath",
            "abc-adroitness-adviser-aftermath",
            "-adroitness-adviser-aftermath",
        ] {
            assert!(
                matches!(
                    WordCode::validate(bad),
                    Err(WordCodeError::InvalidNameplate(_))
                ),
                "expected InvalidNameplate for {bad}"
            );
        }
    }

    #[test]
    fn validate_rejects_duplicate_word() {
        // Review note: duplicate words MUST be rejected.
        assert_eq!(
            WordCode::validate("7-adroitness-adviser-adroitness"),
            Err(WordCodeError::DuplicateWord("adroitness".to_string()))
        );
        // Plan QA scenario: 5 parts ending in a repeated word.
        assert!(
            WordCode::validate("7-adroitness-adviser-aftermath-aftermath").is_err(),
            "repeated word in 5-part code must be rejected"
        );
    }

    #[test]
    fn split_returns_nameplate_and_words() {
        assert_eq!(
            WordCode::split("7-correct-horse-battery"),
            Ok((7, "correct-horse-battery".to_string()))
        );
    }

    #[test]
    fn split_rejects_missing_hyphen() {
        assert_eq!(WordCode::split("7"), Err(WordCodeError::MissingHyphen));
    }

    #[test]
    fn split_rejects_invalid_nameplate() {
        for bad in ["abc-x", "0-x", "-x"] {
            assert!(
                matches!(
                    WordCode::split(bad),
                    Err(WordCodeError::InvalidNameplate(_))
                ),
                "expected InvalidNameplate for {bad}"
            );
        }
    }

    #[test]
    fn split_rejects_empty_words() {
        assert_eq!(WordCode::split("7-"), Err(WordCodeError::EmptyWords));
    }

    #[test]
    fn generate_produces_parseable_codes() {
        for seed in 0..100 {
            let code = WordCode::generate(7, &mut rng(seed)).expect("generate must succeed");
            let parsed = WordCode::validate(&code.to_string()).expect("generated code must parse");
            assert_eq!(parsed, code);
            assert_eq!(parsed.nameplate(), 7);
        }
    }

    #[test]
    fn generate_uses_three_distinct_words() {
        for seed in 0..200 {
            let code = WordCode::generate(7, &mut rng(seed)).expect("generate must succeed");
            let words = code.words();
            assert_ne!(words[0], words[1], "seed {seed}: repeated words");
            assert_ne!(words[0], words[2], "seed {seed}: repeated words");
            assert_ne!(words[1], words[2], "seed {seed}: repeated words");
        }
    }

    #[test]
    fn generate_rejects_invalid_nameplate() {
        for bad in [0, 10000] {
            assert!(
                matches!(
                    WordCode::generate(bad, &mut rng(1)),
                    Err(WordCodeError::InvalidNameplate(_))
                ),
                "expected InvalidNameplate for nameplate {bad}"
            );
        }
    }
}
