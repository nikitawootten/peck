pub mod action;
pub mod extents;
pub mod tree;

use atspi::{ObjectRefOwned, Role, StateSet};

/// One UI element discovered in the active application's subtree.
#[derive(Debug, Clone)]
pub struct Element {
    /// AT-SPI object reference (bus name + object path)
    pub object: ObjectRefOwned,
    /// Accessible role (Button, Link, MenuItem, ...).
    pub role: Role,
    /// Accessible name (label text)
    pub name: String,
    /// Accessible state set (showing, visible, enabled, ...).
    pub states: StateSet,
}
