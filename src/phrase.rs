//! Three-word phrase authentication.
//!
//! `tf recv` shows a fresh phrase like `apple-river-stone` each run and the
//! sender types it. Both sides derive the 32-byte wire token from the phrase,
//! so the protocol in `wire.rs` is unchanged.
//!
//! The entropy argument: a receiver accepts exactly one connection per run,
//! so an attacker on the cable gets a single guess. 256^3 phrases (16.7M)
//! against one guess is comfortable.

use std::io::{self, Read};

use crate::wire::TOKEN_LEN;

/// Common, short, unambiguous words. 256 of them, so a single random byte
/// picks one uniformly with no modulo bias.
const WORDS: [&str; 256] = [
    "apple", "arrow", "atlas", "autumn", "bacon", "badge", "bamboo", "barn", "basil", "beach",
    "beacon", "berry", "birch", "bloom", "breeze", "brick", "bridge", "brook", "butter", "cabin",
    "cactus", "camel", "candle", "canyon", "castle", "cedar", "chalk", "charm", "cherry", "cider",
    "cinder", "circle", "cliff", "cloud", "clover", "coast", "cocoa", "comet", "copper", "coral",
    "cotton", "crater", "creek", "crystal", "daisy", "dance", "dawn", "delta", "desert", "dew",
    "dolphin", "dove", "drift", "drum", "dune", "dusk", "eagle", "echo", "ember", "fable",
    "falcon", "fern", "field", "finch", "flame", "flint", "flood", "flute", "foam", "forest",
    "forge", "fox", "frost", "gale", "garden", "gate", "glacier", "glen", "gold", "goose",
    "granite", "grape", "grass", "grove", "gulf", "hail", "harbor", "hazel", "heron", "hill",
    "honey", "horse", "island", "ivory", "jasmine", "jewel", "juniper", "kayak", "kestrel", "kite",
    "lagoon", "lake", "larch", "laurel", "leaf", "lemon", "lilac", "lily", "linden", "lodge",
    "lotus", "lunar", "maple", "marble", "marsh", "meadow", "melon", "merlin", "mesa", "mist",
    "moss", "moth", "mountain", "mouse", "mushroom", "nectar", "needle", "north", "oak", "oasis",
    "ocean", "olive", "onyx", "opal", "orchid", "otter", "owl", "palm", "panda", "paper", "park",
    "parrot", "path", "peach", "peak", "pearl", "pebble", "pelican", "pepper", "perch", "petal",
    "pine", "plain", "planet", "plow", "plum", "pond", "poplar", "poppy", "prairie", "puffin",
    "quail", "quartz", "quince", "rabbit", "rain", "raven", "reed", "ridge", "river", "robin",
    "rock", "rose", "rowan", "ruby", "sage", "salmon", "sand", "seal", "shadow", "shell", "shore",
    "silk", "silver", "sky", "slate", "smoke", "snow", "solar", "south", "sparrow", "spring",
    "spruce", "star", "steel", "stone", "storm", "stream", "summer", "summit", "sun", "swan",
    "swift", "thorn", "tide", "timber", "topaz", "trail", "trout", "tulip", "tuna", "valley",
    "vapor", "velvet", "vine", "violet", "viper", "walnut", "wasp", "wave", "west", "wheat",
    "willow", "wind", "winter", "wolf", "wren", "yard", "yarn", "yellow", "zebra", "zephyr",
    "zinc", "acorn", "amber", "anchor", "antler", "aspen", "azure", "bison", "blaze", "bluff",
    "boulder", "branch", "bramble", "broom", "carp", "cay", "clay", "cove", "crane", "crow",
    "dale", "dell", "fawn", "hedge",
];

/// A fresh `word-word-word` phrase. Three random bytes, three words.
pub fn generate_phrase() -> io::Result<String> {
    let mut bytes = [0u8; 3];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(format!(
        "{}-{}-{}",
        WORDS[bytes[0] as usize], WORDS[bytes[1] as usize], WORDS[bytes[2] as usize]
    ))
}

/// Derive the 32-byte wire token from a phrase. Case- and
/// surrounding-whitespace-insensitive, since people type these by hand.
pub fn derive_token(phrase: &str) -> [u8; TOKEN_LEN] {
    sha256(phrase.trim().to_lowercase().as_bytes())
}

// SHA-256, written out because the dependency budget is libc + clap and a
// hash is the only crypto this tool needs. Verified against known vectors
// in the tests below.
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        let mut out = String::new();
        for byte in bytes {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Longer than one block, exercises the length padding.
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn generated_phrases_are_three_words_from_the_list() {
        for _ in 0..20 {
            let phrase = generate_phrase().unwrap();
            let parts: Vec<&str> = phrase.split('-').collect();
            assert_eq!(parts.len(), 3, "got {phrase:?}");
            for part in parts {
                assert!(WORDS.contains(&part), "{part:?} not in word list");
            }
        }
    }

    #[test]
    fn derive_token_ignores_case_and_surrounding_space() {
        assert_eq!(
            derive_token("Apple-River-Stone"),
            derive_token("  apple-river-stone  ")
        );
    }

    #[test]
    fn derive_token_differs_between_phrases() {
        assert_ne!(
            derive_token("apple-river-stone"),
            derive_token("apple-river-brook")
        );
    }
}
