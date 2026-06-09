//! wlr-layer-shell overlay: renders hint labels over the focused window.
mod text;

use anyhow::{anyhow, Context, Result};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_registry,
    delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{
        slot::{Buffer, SlotPool},
        Shm, ShmHandler,
    },
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

use crate::hints::Hint;
use crate::instrument::Phase;
use crate::niri::WindowGeometry;

use text::GlyphAtlas;

/// Label glyph height in physical pixels.
const LABEL_PX: f32 = 26.0;
/// Padding inside a label background box, physical pixels.
const LABEL_PAD: i32 = 4;
/// Label background (BGRA, opaque yellow) and foreground (black).
const LABEL_BG: [u8; 4] = [0x00, 0xC8, 0xFF, 0xFF];
const LABEL_FG: [u8; 4] = [0x10, 0x10, 0x10, 0xFF];
/// Already-typed prefix, drawn muted so the remaining keys stand out.
const LABEL_DIM: [u8; 4] = [0x70, 0x70, 0x70, 0xFF];

/// Show hint labels on the focused window's output and read the keyboard: the
/// user types a label to filter and select a target. Returns the selected
/// hint (label + rect + element), or `None` if cancelled (`Esc`).
pub fn select(geom: &WindowGeometry, hints: &[Hint]) -> Result<Option<Hint>> {
    let _p = Phase::start("overlay");

    let conn = Connection::connect_to_env().context("failed to connect to the Wayland display")?;
    let (globals, mut event_queue) =
        registry_queue_init(&conn).context("failed to init Wayland registry")?;
    let qh = event_queue.handle();

    let compositor =
        CompositorState::bind(&globals, &qh).map_err(|e| anyhow!("wl_compositor: {e}"))?;
    let layer_shell =
        LayerShell::bind(&globals, &qh).map_err(|e| anyhow!("wlr-layer-shell unavailable: {e}"))?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| anyhow!("wl_shm: {e}"))?;
    let viewporter: WpViewporter = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| anyhow!("wp-viewporter unavailable (needed for crisp labels): {e}"))?;

    // Pre-build the glyph atlas
    let chars: std::collections::BTreeSet<char> =
        hints.iter().flat_map(|h| h.label.chars()).collect();
    let atlas =
        GlyphAtlas::new(LABEL_PX, chars).context("failed to load a system monospace font")?;

    let mut overlay = Overlay {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        keyboard: None,
        shm,
        pool: None,
        layer: None,
        viewport: None,
        held_buffer: None,
        logical: (0, 0),
        scale: geom.scale,
        atlas,
        hints: hints.to_vec(),
        typed: String::new(),
        selected: None,
        configured: false,
        done: false,
        closed: false,
    };

    event_queue
        .roundtrip(&mut overlay)
        .context("Wayland roundtrip for outputs failed")?;

    let target = overlay
        .output_state
        .outputs()
        .find(|o| {
            overlay.output_state.info(o).and_then(|i| i.name).as_deref() == Some(&geom.output_name)
        })
        .ok_or_else(|| {
            anyhow!(
                "focused output {:?} not found via Wayland",
                geom.output_name
            )
        })?;

    let surface = compositor.create_surface(&qh);
    let viewport = viewporter.get_viewport(&surface, &qh, ());
    let layer = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Overlay,
        Some("peck-hints"),
        Some(&target),
    );
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    // Cover the whole output, ignoring other layers' exclusive zones.
    layer.set_exclusive_zone(-1);
    // Exclusive keyboard: the overlay owns focus so it can READ hint keys
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    // Let the compositor size us to the output.
    layer.set_size(0, 0);

    // Pointer events pass through to the app underneath
    if let Ok(region) = Region::new(&compositor) {
        layer.set_input_region(Some(region.wl_region()));
    }

    layer.commit();
    overlay.layer = Some(layer);
    overlay.viewport = Some(viewport);

    while !overlay.done && !overlay.closed {
        event_queue
            .blocking_dispatch(&mut overlay)
            .context("Wayland dispatch failed")?;
    }

    let selected = overlay.selected.map(|i| overlay.hints[i].clone());

    drop(overlay.layer.take());
    drop(overlay.viewport.take());
    if let Some(kbd) = overlay.keyboard.take() {
        kbd.release();
    }
    let _ = event_queue.roundtrip(&mut overlay);

    Ok(selected)
}

struct Overlay {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    shm: Shm,
    pool: Option<SlotPool>,
    layer: Option<LayerSurface>,
    viewport: Option<WpViewport>,
    /// The presented buffer, held so its slot is not reused while mapped.
    held_buffer: Option<Buffer>,
    /// Logical surface size from the configure.
    logical: (u32, u32),
    scale: f64,
    atlas: GlyphAtlas,
    hints: Vec<Hint>,
    /// Prefix the user has typed so far.
    typed: String,
    /// Index (into `hints`) of the selected target, once resolved.
    selected: Option<usize>,
    /// Whether the first configure has produced a surface size.
    configured: bool,
    /// Set when selection or cancellation finishes the loop.
    done: bool,
    closed: bool,
}

impl Overlay {
    fn draw(&mut self) {
        let (lw, lh) = self.logical;
        if lw == 0 || lh == 0 {
            return;
        }
        // Physical buffer; viewport maps it back down to the logical surface.
        let pw = (lw as f64 * self.scale).round() as u32;
        let ph = (lh as f64 * self.scale).round() as u32;
        let stride = pw as i32 * 4;

        let pool = self.pool.get_or_insert_with(|| {
            SlotPool::new((pw * ph * 4) as usize, &self.shm).expect("create SlotPool")
        });
        let (buffer, canvas) = pool
            .create_buffer(pw as i32, ph as i32, stride, wl_shm::Format::Argb8888)
            .expect("create buffer");

        canvas.fill(0); // transparent
        let mut c = Canvas {
            buf: canvas,
            width: pw,
            height: ph,
        };

        let mut placed: Vec<(i32, i32, i32, i32)> = Vec::with_capacity(self.hints.len());
        for hint in &self.hints {
            // Filter: only labels still matching the typed prefix are shown.
            if !hint.label.starts_with(&self.typed) {
                continue;
            }
            let (tw, th) = self.atlas.measure(&hint.label);
            let (bw, bh) = (tw as i32 + 2 * LABEL_PAD, th as i32 + 2 * LABEL_PAD);

            // Anchor at the element's top-left, clamped on-screen.
            let bx = hint.rect.x.clamp(0, (pw as i32 - bw).max(0));
            let mut by = hint.rect.y.clamp(0, (ph as i32 - bh).max(0));
            // Nudge down to avoid overlapping an already-placed label.
            for _ in 0..16 {
                if !placed.iter().any(|r| overlaps((bx, by, bw, bh), *r)) {
                    break;
                }
                by = (by + bh).min((ph as i32 - bh).max(0));
            }

            c.fill(bx, by, bw, bh, LABEL_BG);
            c.border(bx, by, bw, bh, 1, LABEL_FG);

            // Draw the already-typed prefix muted, the remaining keys in full.
            let split = self.typed.len();
            let (prefix, rest) = hint.label.split_at(split);
            let tx = bx + LABEL_PAD;
            let ty = by + LABEL_PAD;
            self.atlas
                .draw(prefix, tx, ty, LABEL_DIM, LABEL_BG, |x, y, color| {
                    c.put(x, y, color)
                });
            let rx = tx + self.atlas.measure(prefix).0 as i32;
            self.atlas
                .draw(rest, rx, ty, LABEL_FG, LABEL_BG, |x, y, color| {
                    c.put(x, y, color)
                });
            placed.push((bx, by, bw, bh));
        }

        let Some(layer) = &self.layer else { return };
        let surface = layer.wl_surface();
        if let Some(vp) = &self.viewport {
            vp.set_destination(lw as i32, lh as i32);
        }
        surface.damage_buffer(0, 0, pw as i32, ph as i32);
        buffer.attach_to(surface).expect("attach buffer");
        layer.commit();

        self.held_buffer = Some(buffer);
        self.configured = true;
    }

    /// Handle a typed character / control key, updating the filter and possibly
    /// resolving a selection. Redraws when the visible set changes.
    fn handle_key(&mut self, event: &KeyEvent) {
        match event.keysym {
            Keysym::Escape => {
                self.done = true;
            }
            Keysym::BackSpace => {
                if self.typed.pop().is_some() {
                    self.draw();
                }
            }
            _ => {
                let Some(ch) = event.utf8.as_ref().and_then(|s| s.chars().next()) else {
                    return;
                };
                let ch = ch.to_ascii_lowercase();
                let candidate = format!("{}{ch}", self.typed);
                let matches: Vec<usize> = self
                    .hints
                    .iter()
                    .enumerate()
                    .filter(|(_, h)| h.label.starts_with(&candidate))
                    .map(|(i, _)| i)
                    .collect();

                match matches.as_slice() {
                    // No target starts with this prefix — ignore the keystroke.
                    [] => {}
                    // Unambiguous: select it (uniform-width labels are prefix-free).
                    [only] => {
                        self.selected = Some(*only);
                        self.done = true;
                    }
                    _ => {
                        self.typed = candidate;
                        self.draw();
                    }
                }
            }
        }
    }
}

fn overlaps(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

/// A mutable view over the SHM buffer for clipped pixel drawing.
struct Canvas<'a> {
    buf: &'a mut [u8],
    width: u32,
    height: u32,
}

impl Canvas<'_> {
    fn put(&mut self, x: i32, y: i32, color: [u8; 4]) {
        if x < 0 || y < 0 || x as u32 >= self.width || y as u32 >= self.height {
            return;
        }
        let idx = ((y as u32 * self.width + x as u32) * 4) as usize;
        self.buf[idx..idx + 4].copy_from_slice(&color);
    }

    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, c: [u8; 4]) {
        for dy in 0..h {
            for dx in 0..w {
                self.put(x + dx, y + dy, c);
            }
        }
    }

    fn border(&mut self, x: i32, y: i32, w: i32, h: i32, t: i32, c: [u8; 4]) {
        self.fill(x, y, w, t, c);
        self.fill(x, y + h - t, w, t, c);
        self.fill(x, y, t, h, c);
        self.fill(x + w - t, y, t, h, c);
    }
}

impl CompositorHandler for Overlay {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Overlay {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Overlay {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.closed = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        self.logical = (configure.new_size.0.max(1), configure.new_size.1.max(1));
        if !self.configured {
            self.draw();
        }
    }
}

impl ShmHandler for Overlay {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl SeatHandler for Overlay {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            if let Ok(kbd) = self.seat_state.get_keyboard(qh, &seat, None) {
                self.keyboard = Some(kbd);
            }
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            if let Some(kbd) = self.keyboard.take() {
                kbd.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for Overlay {
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.handle_key(&event);
    }

    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }
    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.handle_key(&event);
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
    }
}

impl ProvidesRegistryState for Overlay {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(Overlay);
delegate_output!(Overlay);
delegate_layer!(Overlay);
delegate_shm!(Overlay);
delegate_seat!(Overlay);
delegate_keyboard!(Overlay);
delegate_registry!(Overlay);
wayland_client::delegate_noop!(Overlay: ignore WpViewporter);
wayland_client::delegate_noop!(Overlay: ignore WpViewport);
