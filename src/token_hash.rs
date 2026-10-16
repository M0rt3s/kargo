//! Keyed hashes of bearer tokens, so raw tokens are never kept as map keys.

use std::hash::{BuildHasher, RandomState};
use std::sync::LazyLock;

/// Random per process: hashes cannot be precomputed or compared across runs.
static HASHER: LazyLock<RandomState> = LazyLock::new(RandomState::new);

/// The keyed hash of a token (`None` is the absent token).
pub(crate) fn hash(token: Option<&str>) -> u64 {
    HASHER.hash_one(token)
}

#[cfg(test)]
mod tests {
    use std::hash::{DefaultHasher, Hash, Hasher};

    use super::*;

    #[test]
    fn hashes_are_stable_within_a_process_but_not_the_unkeyed_default() {
        assert_eq!(hash(Some("t")), hash(Some("t")));
        assert_ne!(hash(Some("t")), hash(None));
        let mut unkeyed = DefaultHasher::new();
        Some("t").hash(&mut unkeyed);
        assert_ne!(hash(Some("t")), unkeyed.finish(), "the key must be random, not the fixed default");
    }
}
