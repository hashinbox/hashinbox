//! BIP39 mnemonics of 12 English words: 128 bits of entropy and the first
//! 4 bits of their SHA-256 as a checksum, 132 bits in 11-bit words. A
//! [`Mnemonic`] is a secret: its `Debug` shows nothing of it, and its
//! entropy is zeroized on drop.

use std::{fmt, sync::LazyLock};

use zeroize::Zeroizing;

use crate::{Error, random, sha256};

/// Words in a mnemonic.
pub const WORDS: usize = 12;
/// Bytes of entropy a mnemonic holds.
const ENTROPY: usize = 16;

/// BIP39's English wordlist (`bip-0039/english.txt`, SHA-256
/// `2f5eed53…24dbda`), 2048 words, sorted.
static WORDLIST: LazyLock<Vec<&'static str>> =
    LazyLock::new(|| include_str!("mnemonic/english.txt").lines().collect());

/// 12 BIP39 words. `Display` writes them lower case, joined by single
/// spaces: the normal form [`Mnemonic::parse`] reads back.
pub struct Mnemonic(Zeroizing<[u8; ENTROPY]>);

/// Why text is not a mnemonic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invalid {
    /// Not 12 words.
    WordCount(usize),
    /// Word `n` (from 1) is not in the wordlist.
    UnknownWord(usize),
    /// The words are known, but their checksum does not match.
    Checksum,
}

impl fmt::Display for Invalid {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WordCount(count) => write!(out, "a mnemonic has {WORDS} words, not {count}"),
            Self::UnknownWord(index) => write!(out, "word {index} is not a BIP39 word"),
            Self::Checksum => out.write_str("the words' checksum does not match"),
        }
    }
}

impl std::error::Error for Invalid {}

impl Mnemonic {
    /// A new mnemonic of 128 bits from the OS.
    ///
    /// # Errors
    ///
    /// [`crate::ErrorKind::Random`] when the OS gives no random bytes.
    pub fn generate() -> Result<Self, Error> {
        Ok(Self::from_entropy(random::<ENTROPY>()?))
    }

    /// The mnemonic of `entropy`.
    #[must_use]
    pub fn from_entropy(entropy: [u8; ENTROPY]) -> Self {
        Self(Zeroizing::new(entropy))
    }

    /// The mnemonic `text` is: any whitespace between words, any case.
    ///
    /// # Errors
    ///
    /// [`Invalid`] when it is not 12 known words with a matching checksum.
    pub fn parse(text: &str) -> Result<Self, Invalid> {
        let words: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
        if words.len() != WORDS {
            return Err(Invalid::WordCount(words.len()));
        }
        let mut bits = Zeroizing::new([0_u8; ENTROPY + 1]);
        for (index, word) in words.iter().enumerate() {
            let value = WORDLIST
                .binary_search(&word.as_str())
                .map_err(|_| Invalid::UnknownWord(index + 1))?;
            for bit in 0..11 {
                if (value >> (10 - bit)) & 1 == 1 {
                    let at = index * 11 + bit;
                    bits[at / 8] |= 0x80 >> (at % 8);
                }
            }
        }
        let mut entropy = [0_u8; ENTROPY];
        entropy.copy_from_slice(&bits[..ENTROPY]);
        let mnemonic = Self::from_entropy(entropy);
        if bits[ENTROPY] != mnemonic.checksum() {
            return Err(Invalid::Checksum);
        }
        Ok(mnemonic)
    }

    /// The 4 checksum bits, in the high nibble.
    fn checksum(&self) -> u8 {
        sha256(self.0.as_slice())[0] & 0xf0
    }
}

impl fmt::Display for Mnemonic {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut bits = Zeroizing::new([0_u8; ENTROPY + 1]);
        bits[..ENTROPY].copy_from_slice(self.0.as_slice());
        bits[ENTROPY] = self.checksum();
        for index in 0..WORDS {
            let value = (0..11).fold(0_usize, |value, bit| {
                let at = index * 11 + bit;
                (value << 1) | usize::from((bits[at / 8] >> (7 - at % 8)) & 1)
            });
            if index > 0 {
                out.write_str(" ")?;
            }
            out.write_str(WORDLIST[value])?;
        }
        Ok(())
    }
}

impl fmt::Debug for Mnemonic {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("Mnemonic(…)")
    }
}

#[cfg(test)]
mod tests {
    use super::{Invalid, Mnemonic, WORDLIST};

    /// BIP39's official vectors of 128 bits (`vectors.json`): entropy and
    /// words.
    const VECTORS: [(&str, &str); 4] = [
        (
            "00000000000000000000000000000000",
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon \
             abandon about",
        ),
        (
            "7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f",
            "legal winner thank year wave sausage worth useful legal winner thank yellow",
        ),
        (
            "80808080808080808080808080808080",
            "letter advice cage absurd amount doctor acoustic avoid letter advice cage above",
        ),
        (
            "ffffffffffffffffffffffffffffffff",
            "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo wrong",
        ),
    ];

    #[test]
    fn the_wordlist_is_2048_sorted_words() {
        assert_eq!(WORDLIST.len(), 2048);
        assert!(WORDLIST.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn the_official_vectors_round_trip() {
        for (entropy, words) in VECTORS {
            let entropy: [u8; 16] = hex::decode(entropy).unwrap().try_into().unwrap();
            let mnemonic = Mnemonic::from_entropy(entropy);
            assert_eq!(mnemonic.to_string(), words);
            assert_eq!(*Mnemonic::parse(words).unwrap().0, entropy);
        }
    }

    #[test]
    fn parse_normalises_whitespace_and_case() {
        let typed =
            "  Legal WINNER thank\tyear wave sausage\nworth useful legal winner thank yellow ";
        assert_eq!(Mnemonic::parse(typed).unwrap().to_string(), VECTORS[1].1,);
    }

    #[test]
    fn parse_refuses_a_bad_checksum_an_unknown_word_and_a_wrong_count() {
        let abandon = "abandon ".repeat(11);
        assert_eq!(
            Mnemonic::parse(&format!("{abandon}abandon")).unwrap_err(),
            Invalid::Checksum
        );
        assert_eq!(
            Mnemonic::parse(&format!("{abandon}bitcoins")).unwrap_err(),
            Invalid::UnknownWord(12)
        );
        assert_eq!(
            Mnemonic::parse(&abandon).unwrap_err(),
            Invalid::WordCount(11)
        );
    }

    #[test]
    fn a_generated_one_parses_back_and_debug_hides_it() {
        let mnemonic = Mnemonic::generate().unwrap();
        let words = mnemonic.to_string();
        assert_eq!(Mnemonic::parse(&words).unwrap().to_string(), words);
        assert_eq!(format!("{mnemonic:?}"), "Mnemonic(…)");
    }
}
