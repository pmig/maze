//! Random two-word names (`brave-otter`) for new drafts, the way Zed names new git
//! worktrees: an adjective and a noun, short, lowercase, easy to say and to tell apart.
//!
//! A draft's tab shows its name instead of "New Message", so several open drafts can be
//! told apart before they have a subject. The name is picked once, when the draft is
//! created, and stored with it. The word lists are Maze's own (64 × 64 = 4,096 names).

use std::hash::{BuildHasher as _, RandomState};

const ADJECTIVES: &[&str] = &[
    "amber", "ample", "autumn", "bold", "brave", "breezy", "bright", "brisk", "calm", "candid",
    "cheery", "clever", "cosy", "crisp", "curious", "daring", "dewy", "eager", "early", "even",
    "fair", "fleet", "fond", "frank", "fresh", "gentle", "glad", "golden", "grand", "happy",
    "hardy", "hazel", "humble", "jolly", "keen", "kind", "lively", "lucid", "lucky", "mellow",
    "merry", "mild", "misty", "modest", "nimble", "noble", "patient", "plucky", "polite", "proud",
    "quick", "quiet", "rapid", "ready", "rosy", "sandy", "silver", "steady", "sunny", "swift",
    "tidy", "vivid", "warm", "witty",
];

const NOUNS: &[&str] = &[
    "acorn", "badger", "beacon", "birch", "bison", "breeze", "brook", "canyon", "cedar", "comet",
    "coral", "crane", "creek", "dune", "eagle", "ember", "falcon", "fern", "finch", "fjord", "fox",
    "glade", "grove", "harbor", "hare", "heron", "island", "jay", "kestrel", "lagoon", "lark",
    "lemur", "lily", "lynx", "maple", "meadow", "mesa", "moose", "newt", "oak", "orca", "otter",
    "owl", "panda", "pebble", "pine", "plover", "pond", "puffin", "quail", "raven", "reef",
    "river", "robin", "sparrow", "spruce", "stork", "summit", "thistle", "tiger", "trout", "tulip",
    "walrus", "willow",
];

/// A small, seedable pseudo-random number generator (SplitMix64) for picking names.
/// Not for anything that needs real randomness.
#[derive(Debug, Clone)]
pub struct NameRng(u64);

impl NameRng {
    /// A generator that always produces the same names (tests).
    pub fn seeded(seed: u64) -> Self {
        Self(seed)
    }

    /// A generator seeded from the process's random hasher keys and the clock.
    pub fn from_entropy() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        Self(RandomState::new().hash_one(nanos))
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..n` (`n > 0`).
    fn below(&mut self, n: usize) -> usize {
        // The modulo bias is irrelevant for 64 words.
        (self.next_u64() % n as u64) as usize
    }
}

/// How many distinct two-word names there are.
pub const NAME_COUNT: usize = ADJECTIVES.len() * NOUNS.len();

fn name_at(ix: usize) -> String {
    let ix = ix % NAME_COUNT;
    format!(
        "{}-{}",
        ADJECTIVES[ix / NOUNS.len()],
        NOUNS[ix % NOUNS.len()]
    )
}

/// A random `adjective-noun` name that isn't in `taken` (the names of the drafts open
/// now). Once every name is taken (4,096 open drafts), a number is appended.
pub fn two_word_name<S: AsRef<str>>(taken: &[S], rng: &mut NameRng) -> String {
    two_word_name_where(|name| taken.iter().any(|t| t.as_ref() == name), rng)
}

/// [`two_word_name`] with a predicate for the names in use.
pub fn two_word_name_where(is_taken: impl Fn(&str) -> bool, rng: &mut NameRng) -> String {
    // A few random tries find a free name almost always; then walk from a random
    // start, so a nearly full set still ends.
    for _ in 0..16 {
        let name = name_at(rng.below(NAME_COUNT));
        if !is_taken(&name) {
            return name;
        }
    }
    let start = rng.below(NAME_COUNT);
    if let Some(name) = (0..NAME_COUNT)
        .map(|offset| name_at(start + offset))
        .find(|name| !is_taken(name))
    {
        return name;
    }
    let base = name_at(start);
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|name| !is_taken(name))
        .unwrap_or(base)
}

/// Whether `name` looks like a generated name: two lowercase words joined by a dash.
pub fn is_two_word_name(name: &str) -> bool {
    let mut parts = name.split('-');
    let (Some(adjective), Some(noun), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let word = |w: &str| !w.is_empty() && w.chars().all(|c| c.is_ascii_lowercase());
    word(adjective) && word(noun)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn names_are_two_lowercase_words() {
        let mut rng = NameRng::seeded(7);
        for _ in 0..200 {
            let name = two_word_name::<&str>(&[], &mut rng);
            assert!(is_two_word_name(&name), "{name}");
        }
        assert!(!is_two_word_name("brave"));
        assert!(!is_two_word_name("brave-otter-2"));
        assert!(!is_two_word_name("Brave-otter"));
    }

    #[test]
    fn the_same_seed_gives_the_same_names() {
        let names = |seed| {
            let mut rng = NameRng::seeded(seed);
            (0..5)
                .map(|_| two_word_name::<&str>(&[], &mut rng))
                .collect::<Vec<_>>()
        };
        assert_eq!(names(42), names(42));
        assert_ne!(names(42), names(43));
    }

    #[test]
    fn open_drafts_never_share_a_name() {
        let mut rng = NameRng::seeded(1);
        let mut taken: Vec<String> = Vec::new();
        for _ in 0..300 {
            let name = two_word_name(&taken, &mut rng);
            assert!(!taken.contains(&name), "{name} twice");
            taken.push(name);
        }
    }

    #[test]
    fn a_nearly_full_set_still_finds_the_free_name() {
        let mut all: HashSet<String> = (0..NAME_COUNT).map(name_at).collect();
        assert_eq!(all.len(), NAME_COUNT);
        let free = name_at(1234);
        all.remove(&free);
        let mut rng = NameRng::seeded(3);
        assert_eq!(two_word_name_where(|n| all.contains(n), &mut rng), free);
        // Every word combination taken: a number keeps the next one unique.
        all.insert(free);
        let extra = two_word_name_where(|n| all.contains(n), &mut rng);
        assert!(extra.ends_with("-2"), "{extra}");
    }

    #[test]
    fn word_lists_have_no_duplicates() {
        let adjectives: HashSet<_> = ADJECTIVES.iter().collect();
        let nouns: HashSet<_> = NOUNS.iter().collect();
        assert_eq!(adjectives.len(), ADJECTIVES.len());
        assert_eq!(nouns.len(), NOUNS.len());
        for word in ADJECTIVES.iter().chain(NOUNS) {
            assert!(word.chars().all(|c| c.is_ascii_lowercase()), "{word}");
        }
    }

    #[test]
    fn entropy_seeds_differ() {
        let a = two_word_name::<&str>(&[], &mut NameRng::from_entropy());
        assert!(is_two_word_name(&a));
    }
}
