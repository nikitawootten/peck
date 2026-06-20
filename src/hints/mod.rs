//! Hint label generation and assignment.

mod short;

pub use short::ShortLabeler;

use std::fmt;

use atspi::Role;

use crate::atspi::Element;
use crate::geometry::PhysicalRect;

/// The full lowercase alphabet, ordered by ergonomics.
const ALPHABET: &[u8] = b"hjklasdfgyuiopqwertnmzxcvb";

/// A label assigned to an on-screen target.
#[derive(Debug, Clone)]
pub struct Hint {
    pub label: String,
    pub rect: PhysicalRect,
    pub element: Element,
}

/// What a labeler may know about each target. Targets arrive in
/// accessibility-tree pre-order
#[allow(dead_code)]
pub struct Target<'a> {
    pub name: &'a str,
    pub role: Role,
    pub rect: PhysicalRect,
}

/// A label assignment strategy.
pub trait Labeler {
    fn labels(&self, targets: &[Target]) -> Vec<String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum HintStyle {
    /// Variable-width codes.
    #[default]
    #[value(name = "short")]
    Short,
}

impl HintStyle {
    fn labeler(self) -> &'static dyn Labeler {
        match self {
            HintStyle::Short => &ShortLabeler,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            HintStyle::Short => "short",
        }
    }
}

impl fmt::Display for HintStyle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Assign labels to located elements in accessibility-tree pre-order.
pub fn assign(located: &[(Element, PhysicalRect)], style: HintStyle) -> Vec<Hint> {
    let labeler = style.labeler();
    let targets: Vec<Target> = located
        .iter()
        .map(|(el, rect)| Target {
            name: &el.name,
            role: el.role,
            rect: *rect,
        })
        .collect();

    let labels = labeler.labels(&targets);
    located
        .iter()
        .zip(labels)
        .map(|((element, rect), label)| Hint {
            label,
            rect: *rect,
            element: element.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;

    #[test]
    fn hint_style_round_trips_through_display() {
        for s in HintStyle::value_variants() {
            assert_eq!(HintStyle::from_str(&s.to_string(), false), Ok(*s));
        }
    }
}

/// Shared harness for testing [`Labeler`] implementations.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    pub fn dummy_targets(n: usize) -> Vec<Target<'static>> {
        (0..n)
            .map(|_| Target {
                name: "",
                role: Role::Button,
                rect: PhysicalRect {
                    x: 0,
                    y: 0,
                    w: 1,
                    h: 1,
                },
            })
            .collect()
    }

    pub fn check_invariants(labeler: &dyn Labeler, n: usize) -> Vec<String> {
        let labels = labeler.labels(&dummy_targets(n));
        assert_eq!(labels.len(), n);
        for l in &labels {
            assert!(!l.is_empty());
        }
        for (i, a) in labels.iter().enumerate() {
            for (j, b) in labels.iter().enumerate() {
                if i != j {
                    assert!(!b.starts_with(a.as_str()), "{a:?} prefixes {b:?}");
                }
            }
        }
        labels
    }
}
