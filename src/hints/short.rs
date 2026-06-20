use super::{Labeler, Target, ALPHABET};

/// Variable-width labels based on Tridactyl's `hintnames_short`: enumerate
/// strings over the alphabet in length-then-lexicographic order, skip the
/// first ⌈(n−b)/(b−1)⌉, and take `n`. Each skipped string stops being a
/// label and instead prefixes the longer labels at the tail of the window,
/// so the result is prefix-free with the minimal number of demotions:
/// high-priority targets keep one-key labels until the screen saturates.
pub struct ShortLabeler;

impl Labeler for ShortLabeler {
    fn labels(&self, targets: &[Target]) -> Vec<String> {
        let n = targets.len();
        let b = ALPHABET.len();
        let skip = (n.saturating_sub(b)).div_ceil(b - 1);
        (skip..skip + n).map(nth_label).collect()
    }
}

/// The `i`-th string over [`ALPHABET`] in length-then-lexicographic order:
/// "s", "a", …, "h", "ss", "sa", ….
fn nth_label(mut i: usize) -> String {
    let base = ALPHABET.len();
    let mut len = 1;
    let mut count = base;
    while i >= count {
        i -= count;
        count *= base;
        len += 1;
    }
    let mut chars = vec![0u8; len];
    for pos in (0..len).rev() {
        chars[pos] = ALPHABET[i % base];
        i /= base;
    }
    String::from_utf8(chars).expect("ASCII alphabet")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hints::testing::check_invariants;

    #[test]
    fn matches_uniform_until_alphabet_exhausted() {
        let labels = check_invariants(&ShortLabeler, ALPHABET.len());
        assert!(labels.iter().all(|l| l.len() == 1));
        assert_eq!(labels[0].as_bytes(), &ALPHABET[..1]);
    }

    #[test]
    fn demotes_minimally_when_overflowing() {
        // One over the alphabet: exactly one single-char label is sacrificed
        // (the easiest char becomes the prefix of the two two-char labels).
        let labels = check_invariants(&ShortLabeler, ALPHABET.len() + 1);
        let ones = labels.iter().filter(|l| l.len() == 1).count();
        assert_eq!(ones, ALPHABET.len() - 1);
        assert_eq!(
            labels[0].as_bytes(),
            &ALPHABET[1..2],
            "first surviving single-char label"
        );
        let demoted = ALPHABET[0] as char;
        assert!(labels[ones..].iter().all(|l| l.starts_with(demoted)));
    }

    #[test]
    fn keeps_short_labels_for_high_priority_targets() {
        let labels = check_invariants(&ShortLabeler, 100);
        // Lengths are non-decreasing: rank 0 gets the cheapest code.
        assert!(labels.windows(2).all(|w| w[0].len() <= w[1].len()));
        assert_eq!(labels[0].len(), 1);
    }

    #[test]
    fn prefix_free_across_sizes() {
        let b = ALPHABET.len();
        // Boundaries of the skip formula and of each label-length tier.
        for n in [
            1,
            2,
            b,
            b + 1,
            2 * b,
            100,
            b * b - 1,
            b * b,
            b * b + 1,
            1500,
        ] {
            check_invariants(&ShortLabeler, n);
        }
    }
}
