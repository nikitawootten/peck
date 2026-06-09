//! Hint label generation and assignment.

use crate::atspi::Element;
use crate::geometry::PhysicalRect;

/// Ergonomic, home-row-biased alphabet (à la vimium). Earlier characters are "easier" and are handed out first.
const ALPHABET: &[u8] = b"sadfjklewcmpgh";

/// A label assigned to an on-screen target.
#[derive(Debug, Clone)]
pub struct Hint {
    pub label: String,
    pub rect: PhysicalRect,
    pub element: Element,
}

/// Assign labels to located elements, ordered so prominent targets get the easiest codes.
pub fn assign(located: &[(Element, PhysicalRect)]) -> Vec<Hint> {
    let mut order: Vec<usize> = (0..located.len()).collect();
    // Higher priority first. Tie-break by reading order for stable output.
    order.sort_by(|&a, &b| {
        let (ra, rb) = (&located[a].1, &located[b].1);
        priority(rb)
            .total_cmp(&priority(ra))
            .then((ra.y, ra.x).cmp(&(rb.y, rb.x)))
    });

    let labels = generate(order.len());
    order
        .into_iter()
        .zip(labels)
        .map(|(i, label)| Hint {
            label,
            rect: located[i].1,
            element: located[i].0.clone(),
        })
        .collect()
}

/// Prominence score for a target. Larger on-screen area ranks higher.
fn priority(r: &PhysicalRect) -> f64 {
    f64::from(r.w.max(0)) * f64::from(r.h.max(0))
}

/// Generate `n` distinct fixed-width labels in "easiest first" order.
fn generate(n: usize) -> Vec<String> {
    let base = ALPHABET.len();
    let mut width = 1;
    while base.pow(width as u32) < n.max(1) {
        width += 1;
    }

    (0..n)
        .map(|i| {
            let mut idx = i;
            let mut chars = vec![0u8; width];
            for pos in (0..width).rev() {
                chars[pos] = ALPHABET[idx % base];
                idx /= base;
            }
            String::from_utf8(chars).expect("ASCII alphabet")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_width_until_alphabet_exhausted() {
        let labels = generate(ALPHABET.len());
        assert!(labels.iter().all(|l| l.len() == 1));
        assert_eq!(labels[0], "s");
        let mut uniq = labels.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), labels.len(), "all labels should be unique");
    }

    #[test]
    fn widens_uniformly_when_overflowing() {
        let labels = generate(ALPHABET.len() + 1);
        assert!(labels.iter().all(|l| l.len() == 2));
        // Prefix-free: no label is a prefix of another (guaranteed by uniform width).
        let mut uniq = labels.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), labels.len());
    }
}
