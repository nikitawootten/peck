use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use anyhow::Result;
use gtk::prelude::*;
use gtk::{gdk, glib, pango};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use tokio::sync::{mpsc, oneshot};
use wayland_client::protocol::wl_pointer::Axis;

use super::hints_window::{monitor_for, HintsOverlay};
use super::overlap_area;
use crate::hints::Hint;
use crate::niri::WindowGeometry;
use crate::pointer::{VirtualPointer, BTN_LEFT, BTN_RIGHT};
use crate::session::PanelOutcome;

/// Panel sizing, logical px.
const PANEL_MIN_W: f64 = 360.0;
const PANEL_MAX_W: f64 = 640.0;
const MARGIN: f64 = 8.0;
const MAX_ROWS: usize = 10;
const ROW_H: f64 = 30.0;
const INPUT_H: f64 = 46.0;
const STATUS_H: f64 = 28.0;

/// One wheel detent per scroll keypress (libinput convention).
const SCROLL_STEP: f64 = 15.0;
/// How long a click waits for a second `Return` to become a double click.
const GRACE_MS: u64 = 300;

const STATUS_LEGEND: &str = "↵ click · ⌃↵ right-click · ↵↵ double · ⌃ warp · ⌃hjkl scroll · esc";
const STATUS_GRACE: &str = "↵ again → double-click";

/// The core's view of one target: enough to match, rank, and act on it.
#[derive(Debug, Clone)]
pub struct Item {
    /// Hint label (typing it selects the item).
    pub label: String,
    /// Element center, output-local physical px.
    pub center: (i32, i32),
    /// Fuzzy-search haystack (element name + role).
    pub text: String,
}

/// Non-text keys the GTK shell feeds the core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelKey {
    Return {
        ctrl: bool,
    },
    Escape,
    Up,
    Down,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
    CtrlPressed,
    CtrlReleased,
    /// Any other key press (still disarms a pending Ctrl tap).
    Other,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// The query entry's text changed.
    Query(String),
    Key(PanelKey),
    /// The double-click grace timer fired.
    GraceElapsed,
    /// Fresh items arrived from a re-scan after scrolling.
    NewHints(Vec<Item>),
}

/// Side effects for the shell to execute, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Selection/filter changed: re-render the list and hint chips.
    Redraw,
    /// Clear the query entry (round-trips as an `Event::Query("")`).
    ClearQuery,
    /// Warp the cursor to output-local physical (x, y).
    Warp(i32, i32),
    /// Click `button` at physical (x, y) (the shell warps there first).
    Click { button: u32, at: (i32, i32) },
    /// Scroll the focused window (the shell warps to its center first).
    Scroll { horizontal: bool, amount: f64 },
    /// Start the double-click grace timer.
    ArmGrace,
    /// Scrolling invalidated the element positions: re-scan the window and
    /// feed the result back as [`Event::NewHints`].
    Refetch,
    /// Interaction over.
    Finish(CoreOutcome),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreOutcome {
    Dismissed {
        scrolls: u32,
        warped: bool,
    },
    Clicked {
        index: usize,
        at: (i32, i32),
        double: bool,
    },
    RightClicked {
        index: usize,
        at: (i32, i32),
    },
}

pub struct PanelCore {
    items: Vec<Item>,
    matcher: Matcher,
    /// Entry text, lowercased once at intake (labels and matching are
    /// case-insensitive).
    query: String,
    /// Items whose hint labels start with the query — shown as their own
    /// "hints" section above the fuzzy matches. Empty when the query is.
    label_matches: Vec<usize>,
    /// Fuzzy-ranked items (all of them when the query is empty), excluding
    /// anything already in `label_matches`.
    fuzzy_matches: Vec<usize>,
    /// Cursor position within the combined (labels, then fuzzy) list.
    cursor: usize,
    /// The item whose hint label the query types out *exactly*, if any —
    /// only then does the hint chip light up.
    hint_match: Option<usize>,
    /// Armed by a bare Ctrl press; cleared by any other key.
    ctrl_tap_armed: bool,
    /// `Some(target index)` while the double-click grace window is open.
    grace: Option<usize>,
    /// Scrolling has invalidated the element positions: hints are cleared
    /// and selection disabled until a re-scan (on Ctrl release) lands.
    stale: bool,
    /// A re-scan is in flight (between [`Effect::Refetch`] and
    /// [`Event::NewHints`]) — shown in the status line.
    refetching: bool,
    scrolls: u32,
    warped: bool,
}

impl PanelCore {
    pub fn new(items: Vec<Item>) -> Self {
        let fuzzy_matches = (0..items.len()).collect();
        Self {
            items,
            matcher: Matcher::new(Config::DEFAULT),
            query: String::new(),
            label_matches: Vec::new(),
            fuzzy_matches,
            cursor: 0,
            hint_match: None,
            ctrl_tap_armed: false,
            grace: None,
            stale: false,
            refetching: false,
            scrolls: 0,
            warped: false,
        }
    }

    /// The item the action keys operate on: the grace target while a click is
    /// pending, else a typed-out hint, else the list cursor.
    pub fn selected(&self) -> Option<usize> {
        self.grace
            .or(self.hint_match)
            .or_else(|| self.combined(self.cursor))
    }

    /// Item at `pos` of the combined (labels, then fuzzy) list.
    fn combined(&self, pos: usize) -> Option<usize> {
        self.label_matches
            .iter()
            .chain(&self.fuzzy_matches)
            .nth(pos)
            .copied()
    }

    fn combined_len(&self) -> usize {
        self.label_matches.len() + self.fuzzy_matches.len()
    }

    pub fn on_event(&mut self, event: Event) -> Vec<Effect> {
        // A pending Ctrl tap survives only a bare press+release.
        if !matches!(
            event,
            Event::Key(PanelKey::CtrlPressed)
                | Event::Key(PanelKey::CtrlReleased)
                | Event::GraceElapsed
        ) {
            self.ctrl_tap_armed = false;
        }

        // Grace window open: the click was already sent; only a second Return
        // (double click), Escape, or the timer finish the interaction.
        if let Some(target) = self.grace {
            let at = self.items[target].center;
            return match event {
                Event::Key(PanelKey::Return { ctrl: false }) => vec![
                    Effect::Click {
                        button: BTN_LEFT,
                        at,
                    },
                    Effect::Finish(CoreOutcome::Clicked {
                        index: target,
                        at,
                        double: true,
                    }),
                ],
                Event::GraceElapsed | Event::Key(PanelKey::Escape) => {
                    vec![Effect::Finish(CoreOutcome::Clicked {
                        index: target,
                        at,
                        double: false,
                    })]
                }
                _ => vec![],
            };
        }

        match event {
            Event::GraceElapsed => vec![], // expired timer with no grace open
            Event::Query(q) => {
                self.query = q.to_lowercase();
                self.refilter();
                self.cursor = 0;
                vec![Effect::Redraw]
            }
            Event::NewHints(items) => {
                self.items = items;
                self.stale = false;
                self.refetching = false;
                self.hint_match = None;
                self.refilter();
                self.cursor = 0;
                vec![Effect::Redraw]
            }
            Event::Key(key) => self.on_key(key),
        }
    }

    fn on_key(&mut self, key: PanelKey) -> Vec<Effect> {
        match key {
            PanelKey::Escape => {
                if self.query.is_empty() {
                    vec![Effect::Finish(CoreOutcome::Dismissed {
                        scrolls: self.scrolls,
                        warped: self.warped,
                    })]
                } else {
                    vec![Effect::ClearQuery]
                }
            }
            PanelKey::Return { ctrl } => {
                let Some(index) = self.selected() else {
                    return vec![];
                };
                let at = self.items[index].center;
                if ctrl {
                    vec![
                        Effect::Click {
                            button: BTN_RIGHT,
                            at,
                        },
                        Effect::Finish(CoreOutcome::RightClicked { index, at }),
                    ]
                } else {
                    self.grace = Some(index);
                    vec![
                        Effect::Click {
                            button: BTN_LEFT,
                            at,
                        },
                        Effect::ArmGrace,
                        Effect::Redraw,
                    ]
                }
            }
            PanelKey::Up => self.move_cursor(-1),
            PanelKey::Down => self.move_cursor(1),
            PanelKey::ScrollUp => self.scroll(false, -SCROLL_STEP),
            PanelKey::ScrollDown => self.scroll(false, SCROLL_STEP),
            PanelKey::ScrollLeft => self.scroll(true, -SCROLL_STEP),
            PanelKey::ScrollRight => self.scroll(true, SCROLL_STEP),
            PanelKey::CtrlPressed => {
                self.ctrl_tap_armed = true;
                vec![]
            }
            PanelKey::CtrlReleased => {
                // Letting go of Ctrl after scrolling: positions are invalid,
                // ask for a re-scan.
                if self.stale {
                    self.ctrl_tap_armed = false;
                    self.refetching = true;
                    return vec![Effect::Redraw, Effect::Refetch];
                }
                if !std::mem::take(&mut self.ctrl_tap_armed) {
                    return vec![];
                }
                let Some(index) = self.selected() else {
                    return vec![];
                };
                self.warped = true;
                let (x, y) = self.items[index].center;
                vec![Effect::Warp(x, y)]
            }
            PanelKey::Other => vec![],
        }
    }

    fn move_cursor(&mut self, delta: i32) -> Vec<Effect> {
        if self.combined_len() == 0 {
            return vec![];
        }
        let max = (self.combined_len() - 1) as i32;
        let new = (self.cursor as i32 + delta).clamp(0, max) as usize;
        if new == self.cursor && self.hint_match.is_none() {
            return vec![];
        }
        self.cursor = new;
        // Explicit navigation overrides a hint match.
        self.hint_match = None;
        vec![Effect::Redraw]
    }

    fn scroll(&mut self, horizontal: bool, amount: f64) -> Vec<Effect> {
        self.scrolls += 1;
        // The first scroll invalidates every element position: clear the
        // hints and the list until the post-scroll re-scan arrives.
        if !self.stale {
            self.stale = true;
            self.refilter();
            return vec![Effect::Scroll { horizontal, amount }, Effect::Redraw];
        }
        vec![Effect::Scroll { horizontal, amount }]
    }

    fn refilter(&mut self) {
        // Stale items have invalid positions; nothing is listed or matchable
        // until the re-scan replaces them.
        if self.stale {
            self.hint_match = None;
            self.label_matches.clear();
            self.fuzzy_matches.clear();
            return;
        }

        if self.query.is_empty() {
            self.hint_match = None;
            self.label_matches.clear();
            self.fuzzy_matches = (0..self.items.len()).collect();
            return;
        }

        // Hints the query is typing towards get their own section; the chip
        // highlight engages only once the label is typed out exactly.
        self.label_matches = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| it.label.starts_with(&self.query))
            .map(|(i, _)| i)
            .collect();
        self.hint_match = self
            .label_matches
            .iter()
            .copied()
            .find(|&i| self.items[i].label == self.query);

        let pattern = Pattern::parse(&self.query, CaseMatching::Ignore, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored: Vec<(u32, usize)> = Vec::new();
        for (i, item) in self.items.iter().enumerate() {
            if item.label.starts_with(&self.query) {
                continue; // already listed in the hints section
            }
            if let Some(s) = pattern.score(Utf32Str::new(&item.text, &mut buf), &mut self.matcher) {
                scored.push((s, i));
            }
        }
        // Higher score first; ties keep the original (priority) order.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.fuzzy_matches = scored.into_iter().map(|(_, i)| i).collect();
    }
}

/// Choose the panel's top-left corner (logical px, output-local).
///
/// Horizontally centered on the focused window. Vertically, nine candidate
/// slots spanning the window's height are scored by the lexicographic key
/// (total overlap area with the hint chips, in-window preference, distance
/// from the window's center): a zero-overlap slot always beats a more
/// central one, and among equals the most central wins. When every
/// in-window slot is covered by chips (a dense page), slots beside the
/// window  compete too, so the panel moves off the window entirely if the
/// output has room.
pub fn place_panel(
    panel: (f64, f64),
    window: (f64, f64, f64, f64),
    output: (f64, f64),
    chips: &[(f64, f64, f64, f64)],
) -> (f64, f64) {
    let (pw, ph) = panel;
    let (wx, wy, ww, wh) = window;
    let (ow, oh) = output;
    let clamp_x = |x: f64| x.clamp(MARGIN, (ow - pw - MARGIN).max(MARGIN));
    let clamp_y = |y: f64| y.clamp(MARGIN, (oh - ph - MARGIN).max(MARGIN));

    const SLOTS: usize = 9;
    let cx = clamp_x(wx + (ww - pw) / 2.0);
    let span = (wh - ph).max(0.0);
    let in_window = (0..SLOTS).map(|i| {
        (
            cx,
            clamp_y(wy + span * i as f64 / (SLOTS - 1) as f64),
            false,
        )
    });
    let side_y = clamp_y(wy + (wh - ph) / 2.0);
    let beside = [wx + ww + MARGIN, wx - pw - MARGIN]
        .into_iter()
        .filter(|&x| x >= MARGIN && x + pw <= ow - MARGIN)
        .map(move |x| (x, side_y, true));

    // Scored slot: ((overlap, beside, distance) sort key, (x, y) corner).
    type Scored = ((f64, bool, f64), (f64, f64));
    let (wcx, wcy) = (wx + ww / 2.0, wy + wh / 2.0);
    let mut best: Option<Scored> = None;
    for (x, y, beside) in in_window.chain(beside) {
        let overlap: f64 = chips.iter().map(|c| overlap_area((x, y, pw, ph), *c)).sum();
        let dist = (x + pw / 2.0 - wcx).hypot(y + ph / 2.0 - wcy);
        let key = (overlap, beside, dist);
        if best.as_ref().is_none_or(|b| key < b.0) {
            best = Some((key, (x, y)));
        }
    }
    // SLOTS >= 1, so at least the first in-window slot is always present.
    best.unwrap().1
}

fn scroll_into_view(
    scroller: &gtk::ScrolledWindow,
    list: &gtk::ListBox,
    row: &gtk::ListBoxRow,
) -> bool {
    let Some(b) = row.compute_bounds(list) else {
        return false;
    };
    let (top, bottom) = (f64::from(b.y()), f64::from(b.y() + b.height()));
    let va = scroller.vadjustment();
    if top < va.value() {
        va.set_value(top);
    } else if bottom > va.value() + va.page_size() {
        va.set_value(bottom - va.page_size());
    }
    true
}

/// A non-selectable text row: section header ("hints" / "matches") or the
/// empty-state message.
fn inert_row(text: &str, css_class: &str, xalign: f32) -> gtk::ListBoxRow {
    let label = gtk::Label::new(Some(text));
    label.add_css_class(css_class);
    label.set_xalign(xalign);
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&label));
    row.set_activatable(false);
    row.set_selectable(false);
    row
}

/// An element row: hint chip (cyan only when its label is typed out), name,
/// and a dim role tag.
fn item_row(hint: &Hint, hint_typed_out: bool) -> gtk::ListBoxRow {
    let (label, name, role) = (
        hint.label.as_str(),
        hint.element.name.as_str(),
        format!("{:?}", hint.element.role),
    );
    let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let chip = gtk::Label::new(Some(label));
    chip.add_css_class("peck-chip");
    if hint_typed_out {
        chip.add_css_class("peck-chip-selected");
    }
    let name_label = gtk::Label::new(Some(name));
    name_label.add_css_class("peck-name");
    name_label.set_ellipsize(pango::EllipsizeMode::End);
    name_label.set_hexpand(true);
    name_label.set_xalign(0.0);
    let role_label = gtk::Label::new(Some(role.as_str()));
    role_label.add_css_class("peck-role");
    hbox.append(&chip);
    hbox.append(&name_label);
    hbox.append(&role_label);
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&hbox));
    row
}

/// Build the core's items from located hints.
fn items_from(hints: &[Hint]) -> Vec<Item> {
    hints
        .iter()
        .map(|h| Item {
            label: h.label.clone(),
            center: h.rect.center(),
            text: format!("{} {:?}", h.element.name, h.element.role),
        })
        .collect()
}

/// Run one panel interaction: hints overlay + panel window, until an action
/// or dismissal. Works with an empty `hints` (scroll-only). `refetch` asks
/// the worker thread to re-scan the window after scrolling.
pub async fn run(
    geom: &WindowGeometry,
    hints: Vec<Hint>,
    refetch: mpsc::Sender<super::HintsReply>,
) -> Result<PanelOutcome> {
    super::ensure_css();

    let scale = geom.scale;
    let vp = VirtualPointer::new(geom)?;

    let items = items_from(&hints);

    // Focused window and output rects in output-local logical px.
    let win = (
        geom.content_origin.0,
        geom.content_origin.1,
        f64::from(geom.window_size.0),
        f64::from(geom.window_size.1),
    );
    let out = (
        f64::from(geom.output_mode.0) / scale,
        f64::from(geom.output_mode.1) / scale,
    );
    let win_center_phys =
        crate::geometry::correct((0, 0, geom.window_size.0, geom.window_size.1), geom).center();

    // Panel size and placement (scored against the overlay's measured chip
    // boxes). The panel stays put across re-scans; only its contents change.
    let pw = (win.2 * 0.6)
        .clamp(PANEL_MIN_W, PANEL_MAX_W)
        .min(out.0 - 2.0 * MARGIN);
    let ph = INPUT_H + STATUS_H + hints.len().clamp(1, MAX_ROWS) as f64 * ROW_H;

    // Hints overlay: chips + selection highlight; the panel owns the keyboard.
    // Created before placement so its chip widgets can be measured. Its state
    // is the single owner of the hints the rows/chips/outcome refer to;
    // replaced wholesale (with the core's items) when a re-scan lands.
    let overlay = HintsOverlay::new(geom, hints, KeyboardMode::None)?;
    let (px, py) = place_panel((pw, ph), win, out, &overlay.chip_boxes());
    overlay.state.borrow_mut().avoid = Some((px, py, pw, ph));
    overlay.sync();
    overlay.window.present();

    // Panel window.
    let window = gtk::Window::new();
    window.add_css_class("peck-panel");
    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Left, true);
    window.set_margin(Edge::Top, py.round() as i32);
    window.set_margin(Edge::Left, px.round() as i32);
    window.set_namespace(Some("peck-panel"));
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    if let Some(monitor) = monitor_for(&geom.output_name) {
        window.set_monitor(Some(&monitor));
    }
    window.set_default_size(pw.round() as i32, ph.round() as i32);

    // Click-through, like the hints overlay: the panel often sits over the
    // focused window, and synthetic clicks/scrolls (and real mouse input)
    // must reach the app underneath, not the panel's widgets.
    super::click_through(&window);

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    vbox.set_size_request(pw.round() as i32, ph.round() as i32);

    let entry = gtk::Text::new();
    entry.add_css_class("peck-query");
    entry.set_placeholder_text(Some("type a hint or search…"));
    vbox.append(&entry);

    let list = gtk::ListBox::new();
    list.add_css_class("peck-list");
    list.set_selection_mode(gtk::SelectionMode::Single);
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_child(Some(&list));
    scroller.set_vexpand(true);
    vbox.append(&scroller);

    let status = gtk::Label::new(Some(STATUS_LEGEND));
    status.add_css_class("peck-status");
    status.set_xalign(0.0);
    vbox.append(&status);

    window.set_child(Some(&vbox));

    let core = Rc::new(RefCell::new(PanelCore::new(items)));
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<Event>();

    // Set while a frame-clock callback is pending to scroll the selection
    // into view; keeps render from stacking duplicate callbacks.
    let scroll_armed = Rc::new(Cell::new(false));

    // Signature of what the list last displayed: the row widgets are rebuilt
    // only when this changes, so a pure selection move (e.g. held Down) keeps
    // the existing, already-laid-out rows and scrolls synchronously instead of
    // racing a fresh layout. Fields: (label matches, fuzzy matches, typed-out
    // hint, stale flag, whether any elements exist).
    type RenderSig = (Vec<usize>, Vec<usize>, Option<usize>, bool, bool);
    let last_render: Rc<RefCell<Option<RenderSig>>> = Rc::new(RefCell::new(None));

    // Item index → its laid-out row, rebuilt in lockstep with the list rows.
    // Lets the selection step (which runs every frame, including on moves that
    // don't rebuild) map the core's selected item straight to its row without
    // re-deriving the header-aware row order.
    let rows_by_item: Rc<RefCell<HashMap<usize, gtk::ListBoxRow>>> =
        Rc::new(RefCell::new(HashMap::new()));

    // Render the status line and sectioned list, and sync the hints overlay
    // with the selection. The status text is derived from core state here so
    // no effect arm has to mutate (and later restore) it by hand.
    let render = {
        let core = core.clone();
        let scroll_armed = scroll_armed.clone();
        let last_render = last_render.clone();
        let rows_by_item = rows_by_item.clone();
        let list = list.clone();
        let scroller = scroller.clone();
        let status = status.clone();
        let overlay = overlay.clone();
        move || {
            let core = core.borrow();

            status.set_text(if core.grace.is_some() {
                STATUS_GRACE
            } else if core.refetching {
                "re-scanning…"
            } else {
                STATUS_LEGEND
            });

            let empty = core.label_matches.is_empty() && core.fuzzy_matches.is_empty();
            let hints_empty = overlay.state.borrow().hints.is_empty();

            let changed = match last_render.borrow().as_ref() {
                Some((l, f, h, s, he)) => {
                    l != &core.label_matches
                        || f != &core.fuzzy_matches
                        || *h != core.hint_match
                        || *s != core.stale
                        || *he != hints_empty
                }
                None => true,
            };
            if changed {
                *last_render.borrow_mut() = Some((
                    core.label_matches.clone(),
                    core.fuzzy_matches.clone(),
                    core.hint_match,
                    core.stale,
                    hints_empty,
                ));
                list.remove_all();
                let mut rows = rows_by_item.borrow_mut();
                rows.clear();
                let hints = &overlay.state.borrow().hints;
                if empty {
                    let text = if core.stale {
                        "release ⌃ to re-scan"
                    } else if hints_empty {
                        "no accessible elements — ⌃hjkl scrolls"
                    } else {
                        "no matches"
                    };
                    list.append(&inert_row(text, "peck-empty", 0.5));
                } else {
                    // Hints the query is typing towards, above the fuzzy matches.
                    if !core.label_matches.is_empty() {
                        list.append(&inert_row("hints", "peck-section", 0.0));
                        for &idx in &core.label_matches {
                            let row = item_row(&hints[idx], core.hint_match == Some(idx));
                            list.append(&row);
                            rows.insert(idx, row);
                        }
                        if !core.fuzzy_matches.is_empty() {
                            list.append(&inert_row("matches", "peck-section", 0.0));
                        }
                    }
                    for &idx in &core.fuzzy_matches {
                        let row = item_row(&hints[idx], false);
                        list.append(&row);
                        rows.insert(idx, row);
                    }
                }
            }

            // Select and scroll the cursor row into view
            match core
                .selected()
                .and_then(|idx| rows_by_item.borrow().get(&idx).cloned())
            {
                Some(row) => {
                    list.select_row(Some(&row));
                    if !scroll_into_view(&scroller, &list, &row) && !scroll_armed.replace(true) {
                        let scroller = scroller.clone();
                        let scroll_armed = scroll_armed.clone();
                        list.add_tick_callback(move |list, _clock| {
                            let Some(row) = list.selected_row() else {
                                scroll_armed.set(false);
                                return glib::ControlFlow::Break;
                            };
                            if !scroll_into_view(&scroller, list, &row) {
                                return glib::ControlFlow::Continue; // not laid out yet
                            }
                            scroll_armed.set(false);
                            glib::ControlFlow::Break
                        });
                    }
                }
                None => list.select_row(gtk::ListBoxRow::NONE),
            }

            // Hint chips: filter by the query prefix; green outline on the
            // selection, cyan chip only for a typed-out hint; everything
            // hidden while positions are stale.
            {
                let mut st = overlay.state.borrow_mut();
                st.typed = core.query.clone();
                st.selected = core.selected();
                st.hint_match = core.hint_match;
                st.hidden = core.stale;
            }
            overlay.sync();
        }
    };

    // Wire input → events.
    entry.connect_changed({
        let tx = ev_tx.clone();
        move |e| {
            let _ = tx.send(Event::Query(e.text().to_string()));
        }
    });

    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed({
        let tx = ev_tx.clone();
        move |_, keyval, _, state| {
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            // Intercepted keys never reach the entry; everything else does
            // (but still disarms a pending Ctrl tap via PanelKey::Other).
            let (key, stop) = match keyval {
                gdk::Key::Escape => (PanelKey::Escape, true),
                gdk::Key::Return | gdk::Key::KP_Enter => (PanelKey::Return { ctrl }, true),
                gdk::Key::Up => (PanelKey::Up, true),
                gdk::Key::Down => (PanelKey::Down, true),
                gdk::Key::p | gdk::Key::P if ctrl => (PanelKey::Up, true),
                gdk::Key::n | gdk::Key::N if ctrl => (PanelKey::Down, true),
                gdk::Key::h | gdk::Key::H if ctrl => (PanelKey::ScrollLeft, true),
                gdk::Key::j | gdk::Key::J if ctrl => (PanelKey::ScrollDown, true),
                gdk::Key::k | gdk::Key::K if ctrl => (PanelKey::ScrollUp, true),
                gdk::Key::l | gdk::Key::L if ctrl => (PanelKey::ScrollRight, true),
                gdk::Key::Control_L | gdk::Key::Control_R => (PanelKey::CtrlPressed, false),
                _ => (PanelKey::Other, false),
            };
            let _ = tx.send(Event::Key(key));
            if stop {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    });
    keys.connect_key_released({
        let tx = ev_tx.clone();
        move |_, keyval, _, _| {
            if matches!(keyval, gdk::Key::Control_L | gdk::Key::Control_R) {
                let _ = tx.send(Event::Key(PanelKey::CtrlReleased));
            }
        }
    });
    window.add_controller(keys);

    render();
    window.present();
    entry.grab_focus();

    // Event loop: feed the core, execute its effects.
    let outcome = loop {
        let Some(event) = ev_rx.recv().await else {
            break CoreOutcome::Dismissed {
                scrolls: 0,
                warped: false,
            };
        };
        let effects = core.borrow_mut().on_event(event);
        let mut finished = None;
        for effect in effects {
            match effect {
                Effect::Redraw => render(),
                Effect::ClearQuery => entry.set_text(""),
                Effect::Warp(x, y) => {
                    if let Err(e) = vp.warp(x, y) {
                        tracing::warn!(error = %e, "warp failed");
                    }
                }
                Effect::Click { button, at } => {
                    let result = vp.warp(at.0, at.1).and_then(|()| vp.click(button));
                    if let Err(e) = result {
                        tracing::warn!(error = %e, "click failed");
                    }
                }
                Effect::Scroll { horizontal, amount } => {
                    let axis = if horizontal {
                        Axis::HorizontalScroll
                    } else {
                        Axis::VerticalScroll
                    };
                    let result = vp
                        .warp(win_center_phys.0, win_center_phys.1)
                        .and_then(|()| vp.scroll(axis, amount));
                    if let Err(e) = result {
                        tracing::warn!(error = %e, "scroll failed");
                    }
                }
                Effect::ArmGrace => {
                    let tx = ev_tx.clone();
                    glib::spawn_future_local(async move {
                        glib::timeout_future(Duration::from_millis(GRACE_MS)).await;
                        let _ = tx.send(Event::GraceElapsed);
                    });
                }
                Effect::Refetch => {
                    let (reply_tx, reply_rx) = oneshot::channel();
                    if refetch.send(reply_tx).await.is_ok() {
                        if let Ok(new_hints) = reply_rx.await {
                            let items = items_from(&new_hints);
                            overlay.set_hints(new_hints);
                            let _ = ev_tx.send(Event::NewHints(items));
                        }
                    }
                }
                Effect::Finish(out) => finished = Some(out),
            }
        }
        if let Some(out) = finished {
            break out;
        }
    };

    let result = {
        let hints = &overlay.state.borrow().hints;
        match outcome {
            CoreOutcome::Dismissed { scrolls, warped } => {
                PanelOutcome::Dismissed { scrolls, warped }
            }
            CoreOutcome::Clicked { index, at, double } => PanelOutcome::Clicked {
                element: hints[index].element.clone(),
                at,
                double,
            },
            CoreOutcome::RightClicked { index, at } => PanelOutcome::RightClicked {
                element: hints[index].element.clone(),
                at,
            },
        }
    };

    window.set_visible(false);
    overlay.dismiss().await; // syncs with the compositor
    window.destroy();

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str, center: (i32, i32), text: &str) -> Item {
        Item {
            label: label.into(),
            center,
            text: text.into(),
        }
    }

    /// Three targets with single-char labels, priority order.
    fn core() -> PanelCore {
        PanelCore::new(vec![
            item("s", (10, 10), "Save document Button"),
            item("a", (20, 20), "Settings Button"),
            item("d", (30, 30), "Close Tab Button"),
        ])
    }

    #[test]
    fn empty_query_selects_first_by_priority() {
        let core = core();
        assert_eq!(core.selected(), Some(0));
        assert!(core.label_matches.is_empty());
        assert_eq!(core.fuzzy_matches, [0, 1, 2]);
    }

    #[test]
    fn typed_out_label_engages_hint_match() {
        let mut core = core();
        // "a" fuzzy-matches several items but types out label "a" exactly.
        core.on_event(Event::Query("a".into()));
        assert_eq!(core.hint_match, Some(1));
        assert_eq!(core.selected(), Some(1));
        // The matched hint sits in its own section, not the fuzzy list.
        assert_eq!(core.label_matches, [1]);
        assert!(!core.fuzzy_matches.contains(&1));
    }

    #[test]
    fn label_prefix_lists_hints_without_engaging_the_match() {
        let mut core = PanelCore::new(vec![
            item("sa", (10, 10), "Save document Button"),
            item("sd", (20, 20), "Settings Button"),
            item("fa", (30, 30), "Close Tab Button"),
        ]);
        core.on_event(Event::Query("s".into()));
        // Both "sa" and "sd" are reachable hints: own section, above fuzzy.
        assert_eq!(core.label_matches, [0, 1]);
        assert_eq!(core.hint_match, None, "shortcode not typed out yet");
        // The cursor still selects the first listed hint (green outline).
        assert_eq!(core.selected(), Some(0));
        // Typing the label out engages the hint (cyan chip).
        core.on_event(Event::Query("sd".into()));
        assert_eq!(core.hint_match, Some(1));
        assert_eq!(core.selected(), Some(1));
    }

    #[test]
    fn fuzzy_ranks_when_no_hint_matches() {
        let mut core = core();
        core.on_event(Event::Query("sett".into()));
        assert!(core.label_matches.is_empty());
        assert_eq!(core.selected(), Some(1), "Settings should rank first");
        assert!(!core.fuzzy_matches.is_empty());
    }

    #[test]
    fn arrows_move_cursor_and_override_hint_match() {
        let mut core = core();
        core.on_event(Event::Query("a".into()));
        assert_eq!(core.selected(), Some(1)); // hint match
        let fx = core.on_event(Event::Key(PanelKey::Down));
        assert_eq!(fx, vec![Effect::Redraw]);
        // Cursor moved past the one-entry hint section into the fuzzy list.
        assert_eq!(core.hint_match, None);
        assert_eq!(core.selected(), core.fuzzy_matches.first().copied());
    }

    #[test]
    fn return_clicks_and_arms_grace() {
        let mut core = core();
        let fx = core.on_event(Event::Key(PanelKey::Return { ctrl: false }));
        assert_eq!(
            fx,
            vec![
                Effect::Click {
                    button: BTN_LEFT,
                    at: (10, 10)
                },
                Effect::ArmGrace,
                Effect::Redraw,
            ]
        );
        // Grace expiry finishes as a single click.
        let fx = core.on_event(Event::GraceElapsed);
        assert_eq!(
            fx,
            vec![Effect::Finish(CoreOutcome::Clicked {
                index: 0,
                at: (10, 10),
                double: false
            })]
        );
    }

    #[test]
    fn second_return_in_grace_double_clicks() {
        let mut core = core();
        core.on_event(Event::Key(PanelKey::Return { ctrl: false }));
        let fx = core.on_event(Event::Key(PanelKey::Return { ctrl: false }));
        assert_eq!(
            fx,
            vec![
                Effect::Click {
                    button: BTN_LEFT,
                    at: (10, 10)
                },
                Effect::Finish(CoreOutcome::Clicked {
                    index: 0,
                    at: (10, 10),
                    double: true
                }),
            ]
        );
    }

    #[test]
    fn grace_ignores_other_input_and_stale_timer_is_noop() {
        let mut graced = core();
        graced.on_event(Event::Key(PanelKey::Return { ctrl: false }));
        assert_eq!(graced.on_event(Event::Key(PanelKey::Down)), vec![]);
        assert_eq!(graced.on_event(Event::Query("x".into())), vec![]);
        // A stale GraceElapsed when no grace is pending does nothing.
        let mut idle = core();
        assert_eq!(idle.on_event(Event::GraceElapsed), vec![]);
    }

    #[test]
    fn ctrl_return_right_clicks_and_finishes() {
        let mut core = core();
        let fx = core.on_event(Event::Key(PanelKey::Return { ctrl: true }));
        assert_eq!(
            fx,
            vec![
                Effect::Click {
                    button: BTN_RIGHT,
                    at: (10, 10)
                },
                Effect::Finish(CoreOutcome::RightClicked {
                    index: 0,
                    at: (10, 10)
                }),
            ]
        );
    }

    #[test]
    fn ctrl_tap_warps_without_finishing() {
        let mut core = core();
        assert_eq!(core.on_event(Event::Key(PanelKey::CtrlPressed)), vec![]);
        let fx = core.on_event(Event::Key(PanelKey::CtrlReleased));
        assert_eq!(fx, vec![Effect::Warp(10, 10)]);
        // Panel still open; dismissing reports the warp.
        let fx = core.on_event(Event::Key(PanelKey::Escape));
        assert_eq!(
            fx,
            vec![Effect::Finish(CoreOutcome::Dismissed {
                scrolls: 0,
                warped: true
            })]
        );
    }

    #[test]
    fn ctrl_chord_does_not_warp() {
        let mut core = core();
        core.on_event(Event::Key(PanelKey::CtrlPressed));
        core.on_event(Event::Key(PanelKey::ScrollDown)); // Ctrl+J
        let fx = core.on_event(Event::Key(PanelKey::CtrlReleased));
        assert!(
            !fx.iter().any(|e| matches!(e, Effect::Warp(..))),
            "a Ctrl chord must not count as a tap"
        );
    }

    #[test]
    fn scroll_keys_emit_scrolls_and_are_counted() {
        let mut core = core();
        // The first scroll also clears the now-invalid hints (Redraw).
        let fx = core.on_event(Event::Key(PanelKey::ScrollDown));
        assert_eq!(
            fx,
            vec![
                Effect::Scroll {
                    horizontal: false,
                    amount: SCROLL_STEP
                },
                Effect::Redraw,
            ]
        );
        let fx = core.on_event(Event::Key(PanelKey::ScrollLeft));
        assert_eq!(
            fx,
            vec![Effect::Scroll {
                horizontal: true,
                amount: -SCROLL_STEP
            }]
        );
        let fx = core.on_event(Event::Key(PanelKey::Escape));
        assert_eq!(
            fx,
            vec![Effect::Finish(CoreOutcome::Dismissed {
                scrolls: 2,
                warped: false
            })]
        );
    }

    #[test]
    fn scrolling_invalidates_hints_until_rescan() {
        let mut core = core();
        core.on_event(Event::Key(PanelKey::CtrlPressed));
        core.on_event(Event::Key(PanelKey::ScrollDown));
        // Stale: nothing listed, nothing selectable, typing changes nothing.
        assert!(core.stale);
        assert!(core.label_matches.is_empty() && core.fuzzy_matches.is_empty());
        assert_eq!(core.selected(), None);
        core.on_event(Event::Query("a".into()));
        assert_eq!(core.selected(), None, "stale items must not be matchable");
        assert_eq!(
            core.on_event(Event::Key(PanelKey::Return { ctrl: false })),
            vec![]
        );
        // Releasing Ctrl asks for a re-scan instead of warping.
        let fx = core.on_event(Event::Key(PanelKey::CtrlReleased));
        assert_eq!(fx, vec![Effect::Redraw, Effect::Refetch]);
    }

    #[test]
    fn new_hints_replace_stale_items_and_reapply_the_query() {
        let mut core = core();
        core.on_event(Event::Query("sett".into()));
        core.on_event(Event::Key(PanelKey::CtrlPressed));
        core.on_event(Event::Key(PanelKey::ScrollDown));
        core.on_event(Event::Key(PanelKey::CtrlReleased));
        // Fresh scan arrives (different positions, new element first).
        let fx = core.on_event(Event::NewHints(vec![
            item("s", (5, 500), "Better Settings Button"),
            item("a", (15, 515), "Save document Button"),
        ]));
        assert_eq!(fx, vec![Effect::Redraw]);
        assert!(!core.stale);
        // The standing query "sett" is reapplied to the new items.
        assert_eq!(core.selected(), Some(0));
        assert_eq!(core.items[0].center, (5, 500));
    }

    #[test]
    fn escape_clears_query_first_then_dismisses() {
        let mut core = core();
        core.on_event(Event::Query("xyz".into()));
        assert_eq!(
            core.on_event(Event::Key(PanelKey::Escape)),
            vec![Effect::ClearQuery]
        );
        // The entry round-trips the clear as an empty Query event.
        core.on_event(Event::Query(String::new()));
        assert_eq!(
            core.on_event(Event::Key(PanelKey::Escape)),
            vec![Effect::Finish(CoreOutcome::Dismissed {
                scrolls: 0,
                warped: false
            })]
        );
    }

    #[test]
    fn empty_panel_scrolls_but_never_clicks() {
        let mut core = PanelCore::new(Vec::new());
        assert_eq!(core.selected(), None);
        assert_eq!(
            core.on_event(Event::Key(PanelKey::Return { ctrl: false })),
            vec![]
        );
        let fx = core.on_event(Event::Key(PanelKey::ScrollDown));
        assert!(matches!(
            fx.as_slice(),
            [Effect::Scroll { .. }, Effect::Redraw]
        ));
    }

    // -- placement --

    #[test]
    fn placement_centers_without_chips() {
        let (x, y) = place_panel(
            (400.0, 300.0),
            (100.0, 50.0, 800.0, 600.0),
            (1920.0, 1080.0),
            &[],
        );
        assert_eq!(x, 100.0 + (800.0 - 400.0) / 2.0);
        assert_eq!(y, 50.0 + (600.0 - 300.0) / 2.0);
    }

    #[test]
    fn placement_avoids_chip_dense_center() {
        // Chips blanket the window's vertical middle; the panel must move off it.
        let chips: Vec<(f64, f64, f64, f64)> = (0..20)
            .map(|i| {
                (
                    120.0 + (i % 5) as f64 * 150.0,
                    250.0 + (i / 5) as f64 * 40.0,
                    40.0,
                    25.0,
                )
            })
            .collect();
        let window = (100.0, 50.0, 800.0, 600.0);
        let (x, y) = place_panel((400.0, 200.0), window, (1920.0, 1080.0), &chips);
        let panel = (x, y, 400.0, 200.0);
        let overlap: f64 = chips.iter().map(|c| overlap_area(panel, *c)).sum();
        assert_eq!(overlap, 0.0, "a zero-overlap slot exists and must win");
    }

    #[test]
    fn placement_falls_back_beside_dense_window() {
        // One chip blankets the whole window: no in-window slot is clean, so
        // the panel moves beside the window — right when there's room.
        let window = (100.0, 50.0, 800.0, 600.0);
        let chips = vec![window];
        let (x, y) = place_panel((400.0, 200.0), window, (1920.0, 1080.0), &chips);
        assert_eq!(x, 100.0 + 800.0 + MARGIN);
        assert_eq!(y, 50.0 + (600.0 - 200.0) / 2.0);

        // No room on the right: fall back to the left side.
        let window = (1112.0, 50.0, 800.0, 600.0);
        let chips = vec![window];
        let (x, _) = place_panel((400.0, 200.0), window, (1920.0, 1080.0), &chips);
        assert_eq!(x, 1112.0 - 400.0 - MARGIN);
    }

    #[test]
    fn placement_dense_fullscreen_window_stays_on_output() {
        // Window covers the output and is blanketed by chips: no side fits,
        // so the least-overlap in-window slot wins and stays on the output.
        let window = (0.0, 0.0, 1920.0, 1080.0);
        let chips = vec![window];
        let (x, y) = place_panel((400.0, 200.0), window, (1920.0, 1080.0), &chips);
        assert!(x >= MARGIN && x + 400.0 <= 1920.0 - MARGIN + 1e-9);
        assert!(y >= MARGIN && y + 200.0 <= 1080.0 - MARGIN + 1e-9);
    }

    #[test]
    fn placement_prefers_in_window_over_beside() {
        // A clean in-window slot exists: the side fallback must not win even
        // though it would also have zero overlap.
        let window = (100.0, 50.0, 800.0, 600.0);
        let chips = vec![(100.0, 50.0, 800.0, 200.0)]; // top third only
        let (x, y) = place_panel((400.0, 200.0), window, (1920.0, 1080.0), &chips);
        assert_eq!(
            x,
            100.0 + (800.0 - 400.0) / 2.0,
            "stays centered on the window"
        );
        let overlap = overlap_area((x, y, 400.0, 200.0), chips[0]);
        assert_eq!(overlap, 0.0);
    }

    #[test]
    fn placement_clamps_to_output() {
        // A window hanging off the output's left edge.
        let (x, y) = place_panel(
            (400.0, 300.0),
            (-300.0, -100.0, 500.0, 350.0),
            (1920.0, 1080.0),
            &[],
        );
        assert!(x >= MARGIN);
        assert!(y >= MARGIN);
        // And a tiny output: the panel still stays on it.
        let (x2, y2) = place_panel(
            (400.0, 300.0),
            (0.0, 0.0, 500.0, 350.0),
            (420.0, 320.0),
            &[],
        );
        assert!(x2 >= MARGIN && y2 >= MARGIN);
        assert!(x2 + 400.0 <= 420.0 - MARGIN + 1e-9);
        assert!(y2 + 300.0 <= 320.0 - MARGIN + 1e-9);
    }
}
