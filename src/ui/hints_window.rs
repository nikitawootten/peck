//! Fullscreen hint overlay: a transparent, click-through `Layer::Overlay`
//! window placing hint-chip widgets over the focused window's actionable
//! elements.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use anyhow::Result;
use gtk::prelude::*;
use gtk::{gdk, glib, pango};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::hints::Hint;
use crate::niri::WindowGeometry;

/// Alpha for the already-typed prefix of a chip label: same colour, faded.
const TYPED_PREFIX_ALPHA: u16 = u16::MAX / 2;

pub struct State {
    pub hints: Vec<Hint>,
    /// Physical→logical divisor for hint rects.
    pub scale: f64,
    /// Chips are shown only for labels starting with this prefix.
    pub typed: String,
    /// Index into `hints` whose element is outlined (green) as selected.
    pub selected: Option<usize>,
    /// Index whose hint label has been typed out (cyan chip).
    pub hint_match: Option<usize>,
    /// Hide all chips/outlines (panel mode: positions stale after a scroll).
    pub hidden: bool,
    /// Logical rect chips must not overlap (the panel), if any.
    pub avoid: Option<(f64, f64, f64, f64)>,
}

#[derive(Clone)]
pub struct HintsOverlay {
    pub window: gtk::Window,
    pub state: Rc<RefCell<State>>,
    size: (f64, f64),
    fixed: gtk::Fixed,
    chips: Rc<RefCell<Vec<gtk::Label>>>,
    /// .peck-outline` border around the selected element.
    outline: gtk::Box,
}

impl HintsOverlay {
    /// Create (but do not present) the overlay on the focused window's output.
    pub fn new(geom: &WindowGeometry, hints: Vec<Hint>, keyboard: KeyboardMode) -> Result<Self> {
        // Chips are measured (for placement) before the first map, so the
        // CSS that sizes them must be installed up front.
        super::ensure_css();

        let state = Rc::new(RefCell::new(State {
            hints,
            scale: geom.scale,
            typed: String::new(),
            selected: None,
            hint_match: None,
            hidden: false,
            avoid: None,
        }));

        let window = gtk::Window::new();
        window.add_css_class("peck-hints");
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(edge, true);
        }
        // Cover the whole output, ignoring other layers' exclusive zones.
        window.set_exclusive_zone(-1);
        window.set_namespace(Some("peck-hints"));
        window.set_keyboard_mode(keyboard);
        if let Some(monitor) = monitor_for(&geom.output_name) {
            window.set_monitor(Some(&monitor));
        } else {
            tracing::warn!(
                output = %geom.output_name,
                "focused output not found among GDK monitors; using the default"
            );
        }

        let fixed = gtk::Fixed::new();
        window.set_child(Some(&fixed));

        // Pointer events pass through to the app underneath.
        super::click_through(&window);

        let outline = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        outline.add_css_class("peck-outline");
        outline.set_visible(false);

        let this = Self {
            window,
            state,
            size: (
                f64::from(geom.output_mode.0) / geom.scale,
                f64::from(geom.output_mode.1) / geom.scale,
            ),
            fixed,
            chips: Rc::new(RefCell::new(Vec::new())),
            outline,
        };
        this.rebuild_chips();
        this.sync();
        Ok(this)
    }

    /// Replace the hint set (after a re-scan), rebuilding the chip widgets.
    pub fn set_hints(&self, hints: Vec<Hint>) {
        self.state.borrow_mut().hints = hints;
        self.rebuild_chips();
        self.sync();
    }

    /// One label per hint; the outline is re-added last to stay on top,
    /// matching the old paint order.
    fn rebuild_chips(&self) {
        let mut chips = self.chips.borrow_mut();
        for chip in chips.drain(..) {
            self.fixed.remove(&chip);
        }
        if self.outline.parent().is_some() {
            self.fixed.remove(&self.outline);
        }
        for hint in &self.state.borrow().hints {
            let chip = gtk::Label::new(Some(hint.label.as_str()));
            chip.add_css_class("peck-chip");
            chip.add_css_class("peck-chip-overlay");
            self.fixed.put(&chip, 0.0, 0.0);
            chips.push(chip);
        }
        self.fixed.put(&self.outline, 0.0, 0.0);
    }

    /// The chips' boxes (logical px) anchored at their elements, pre-nudge —
    /// placement scoring input for the panel.
    pub fn chip_boxes(&self) -> Vec<(f64, f64, f64, f64)> {
        let st = self.state.borrow();
        let chips = self.chips.borrow();
        st.hints
            .iter()
            .zip(chips.iter())
            .map(|(hint, chip)| {
                let (w, h) = natural_size(chip);
                (
                    hint.rect.x as f64 / st.scale,
                    hint.rect.y as f64 / st.scale,
                    w,
                    h,
                )
            })
            .collect()
    }

    /// Bring the widgets in line with [`State`]: filter chips by the typed
    /// prefix (rendering the prefix faded), recolour a typed-out chip, place
    /// chips avoiding each other and the panel, and outline the selection.
    pub fn sync(&self) {
        let st = self.state.borrow();
        let chips = self.chips.borrow();

        let (width, height) = self.size;
        let mut placed: Vec<(f64, f64, f64, f64)> = st.avoid.into_iter().collect();
        for (i, (hint, chip)) in st.hints.iter().zip(chips.iter()).enumerate() {
            // Filter: only labels still matching the typed prefix are shown
            if st.hidden || !hint.label.starts_with(&st.typed) {
                chip.set_visible(false);
                continue;
            }
            chip.set_visible(true);

            // Fade the already-typed prefix
            if st.typed.is_empty() {
                chip.set_attributes(None);
            } else {
                let attrs = pango::AttrList::new();
                let mut dim = pango::AttrInt::new_foreground_alpha(TYPED_PREFIX_ALPHA);
                dim.set_end_index(st.typed.len() as u32);
                attrs.insert(dim);
                chip.set_attributes(Some(&attrs));
            }

            if st.hint_match == Some(i) {
                chip.add_css_class("peck-chip-selected");
            } else {
                chip.remove_css_class("peck-chip-selected");
            }

            // Anchor at the element's top-left, clamped on-screen; nudge down
            // to avoid overlapping an already-placed chip (or the panel).
            let (bw, bh) = natural_size(chip);
            let bx = (hint.rect.x as f64 / st.scale).clamp(0.0, (width - bw).max(0.0));
            let mut by = (hint.rect.y as f64 / st.scale).clamp(0.0, (height - bh).max(0.0));
            for _ in 0..16 {
                if !placed.iter().any(|r| overlaps((bx, by, bw, bh), *r)) {
                    break;
                }
                by = (by + bh).min((height - bh).max(0.0));
            }
            self.fixed.move_(chip, bx, by);
            placed.push((bx, by, bw, bh));
        }

        // Outline the selected element's rect
        let selected = (!st.hidden).then_some(st.selected).flatten();
        match selected.and_then(|i| st.hints.get(i)) {
            Some(hint) => {
                let r = &hint.rect;
                let s = st.scale;
                self.outline.set_size_request(
                    (r.w as f64 / s).round() as i32,
                    (r.h as f64 / s).round() as i32,
                );
                self.fixed
                    .move_(&self.outline, r.x as f64 / s - 2.0, r.y as f64 / s - 2.0);
                self.outline.set_visible(true);
            }
            None => self.outline.set_visible(false),
        }
    }

    /// Hide the overlay and wait until the compositor has processed the unmap,
    /// so a synthetic gesture or AT-SPI action lands on the real surface with
    /// keyboard focus restored to the app.
    pub async fn dismiss(self) {
        self.window.set_visible(false);
        // Let GTK flush the unmap commit, then sync with the server.
        glib::timeout_future(Duration::from_millis(15)).await;
        if let Some(display) = gdk::Display::default() {
            display.sync();
        }
        self.window.destroy();
    }
}

/// Show hint labels and read the keyboard: the user types a label to filter
/// and select a target. Returns the selected hint, or `None` on Esc.
pub async fn select(geom: &WindowGeometry, hints: Vec<Hint>) -> Result<Option<Hint>> {
    let overlay = HintsOverlay::new(geom, hints, KeyboardMode::Exclusive)?;
    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<Option<usize>>(1);

    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed({
        let overlay = overlay.clone();
        move |_, keyval, _, _| {
            // Bind first: the RefMut must drop before sync() re-borrows.
            let outcome = handle_gesture_key(&mut overlay.state.borrow_mut(), keyval);
            match outcome {
                KeyOutcome::Pending => {}
                KeyOutcome::Redraw => overlay.sync(),
                KeyOutcome::Finished(sel) => {
                    let _ = done_tx.try_send(sel);
                }
            }
            // Exclusive keyboard: the overlay owns every key while mapped.
            glib::Propagation::Stop
        }
    });
    overlay.window.add_controller(keys);
    overlay.window.present();

    let selected = done_rx.recv().await.flatten();
    let hint = selected.map(|i| overlay.state.borrow().hints[i].clone());
    overlay.dismiss().await;
    Ok(hint)
}

enum KeyOutcome {
    Pending,
    Redraw,
    Finished(Option<usize>),
}

/// Gesture-mode key semantics: Esc cancels, Backspace pops, a printable char
/// narrows the prefix (0 matches → ignored, 1 → selected, n → redraw).
fn handle_gesture_key(state: &mut State, keyval: gdk::Key) -> KeyOutcome {
    match keyval {
        gdk::Key::Escape => KeyOutcome::Finished(None),
        gdk::Key::BackSpace => {
            if state.typed.pop().is_some() {
                KeyOutcome::Redraw
            } else {
                KeyOutcome::Pending
            }
        }
        _ => {
            let Some(ch) = keyval.to_unicode().filter(|c| c.is_ascii_alphanumeric()) else {
                return KeyOutcome::Pending;
            };
            let candidate = format!("{}{}", state.typed, ch.to_ascii_lowercase());
            let matches: Vec<usize> = state
                .hints
                .iter()
                .enumerate()
                .filter(|(_, h)| h.label.starts_with(&candidate))
                .map(|(i, _)| i)
                .collect();
            match matches.as_slice() {
                // No target starts with this prefix — ignore the keystroke.
                [] => KeyOutcome::Pending,
                // Unambiguous: select it (uniform-width labels are prefix-free).
                [only] => KeyOutcome::Finished(Some(*only)),
                _ => {
                    state.typed = candidate;
                    KeyOutcome::Redraw
                }
            }
        }
    }
}

/// A label's natural size, CSS padding/border included, logical px.
fn natural_size(label: &gtk::Label) -> (f64, f64) {
    let (_, w, _, _) = label.measure(gtk::Orientation::Horizontal, -1);
    let (_, h, _, _) = label.measure(gtk::Orientation::Vertical, w);
    (f64::from(w), f64::from(h))
}

fn overlaps(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> bool {
    super::overlap_area(a, b) > 0.0
}

/// Find the GDK monitor whose connector matches a niri output name.
pub(super) fn monitor_for(connector: &str) -> Option<gdk::Monitor> {
    let display = gdk::Display::default()?;
    let monitors = display.monitors();
    (0..monitors.n_items())
        .filter_map(|i| monitors.item(i)?.downcast::<gdk::Monitor>().ok())
        .find(|m| m.connector().as_deref() == Some(connector))
}
