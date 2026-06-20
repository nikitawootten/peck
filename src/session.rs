use anyhow::{Context, Result};
use atspi::AccessibilityConnection;

use std::fmt;

use crate::atspi::{action, extents, tree, Element};
use crate::geometry::{self, PhysicalRect};
use crate::hints::{self, Hint, HintStyle};
use crate::instrument::Phase;
use crate::niri::{self, WindowGeometry};
use crate::pointer::{self, Mode};
use crate::ui::UiHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Request {
    /// Click the selected element (prefers its semantic AT-SPI action).
    #[value(name = "left_click")]
    LeftClick,
    /// Right-click the selected element.
    #[value(name = "right_click")]
    RightClick,
    /// Middle-click the selected element.
    #[value(name = "middle_click")]
    MiddleClick,
    /// Double-click the selected element.
    #[value(name = "double_click")]
    DoubleClick,
    /// Warp the cursor to the selected element without clicking.
    #[value(name = "warp")]
    Warp,
    /// Open the panel: a fuzzy-searchable element list plus hints; the chosen
    /// element is highlighted first and acted on with a second key.
    #[default]
    #[value(name = "panel")]
    Panel,
}

impl Request {
    /// The gesture this request dispatches, or `None` for the panel.
    pub fn gesture(self) -> Option<Mode> {
        match self {
            Request::LeftClick => Some(Mode::LeftClick),
            Request::RightClick => Some(Mode::RightClick),
            Request::MiddleClick => Some(Mode::MiddleClick),
            Request::DoubleClick => Some(Mode::DoubleClick),
            Request::Warp => Some(Mode::Warp),
            Request::Panel => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Request::LeftClick => "left_click",
            Request::RightClick => "right_click",
            Request::MiddleClick => "middle_click",
            Request::DoubleClick => "double_click",
            Request::Warp => "warp",
            Request::Panel => "panel",
        }
    }
}

impl fmt::Display for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub struct Session {
    a11y: AccessibilityConnection,
}

impl Session {
    /// Connect to the accessibility bus and verify accessibility is enabled.
    pub async fn new() -> Result<Self> {
        let _p = Phase::start("connect");

        let a11y = AccessibilityConnection::new()
            .await
            .context("could not connect to the AT-SPI accessibility bus.")?;

        Ok(Self { a11y })
    }

    /// Enumerate the actionable elements in the focused window's accessibility
    /// subtree (the per-invocation "fetch" phase).
    pub async fn actionable_elements(&self) -> Result<Vec<Element>> {
        let _p = Phase::start("tree");
        tree::actionable_elements(&self.a11y).await
    }

    /// Enumerate actionable elements and resolve each to output-local physical
    /// coordinates: AT-SPI window-relative extents corrected via niri's
    /// focused-window geometry.
    pub async fn located_elements(&self) -> Result<(WindowGeometry, Vec<(Element, PhysicalRect)>)> {
        let geom = {
            let _p = Phase::start("niri");
            niri::focused_window_geometry().context("failed to resolve focused-window geometry")?
        };
        let located = self.located_in(&geom).await?;
        Ok((geom, located))
    }

    /// Enumerate actionable elements within an already-resolved window
    /// geometry. Errors when the focused window exposes no accessibility
    /// frame — panel mode treats that as an empty element list instead.
    async fn located_in(&self, geom: &WindowGeometry) -> Result<Vec<(Element, PhysicalRect)>> {
        let frame = tree::active_frame(&self.a11y)
            .await?
            .context("no active toplevel frame found (is a window focused, and a11y enabled?)")?;
        self.located_in_frame(geom, frame).await
    }

    /// Enumerate actionable elements under a known toplevel frame.
    async fn located_in_frame(
        &self,
        geom: &WindowGeometry,
        frame: atspi::ObjectRefOwned,
    ) -> Result<Vec<(Element, PhysicalRect)>> {
        // Some toolkits (Firefox/Wayland) report window-relative coordinates
        // against their CSD-shadow-inclusive *surface*, so the frame's own
        // window-relative origin is non-zero (e.g. (20,20)) while niri's window
        // geometry excludes the shadow. Subtract the frame origin from every
        // element so both agree on "window-relative". For GTK (frame at (0,0))
        // this is a no-op.
        let (fx, fy) = {
            let _p = Phase::start("frame-origin");
            extents::window_extents(&self.a11y, &frame)
                .await
                .map(|(x, y, _, _)| (x, y))
                .unwrap_or((0, 0))
        };

        let elements = {
            let _p = Phase::start("tree");
            tree::walk(&self.a11y, frame).await?
        };

        let _p = Phase::start("extents");
        let mut located = Vec::with_capacity(elements.len());
        for el in elements {
            match extents::window_extents(&self.a11y, &el.object).await {
                Ok((x, y, w, h)) => {
                    let rect = geometry::correct((x - fx, y - fy, w, h), geom);
                    located.push((el, rect));
                }
                Err(e) => {
                    tracing::debug!(name = %el.name, error = %e, "skipping element without extents");
                }
            }
        }

        Ok(located)
    }

    /// Run one full activation, shared by `oneshot` and the daemon (which
    /// calls it once per `peck activate` request, reusing the warm AT-SPI
    /// connection this `Session` already holds).
    pub async fn activate(
        &self,
        ui: &UiHandle,
        request: Request,
        style: HintStyle,
    ) -> Result<Activation> {
        match request.gesture() {
            Some(mode) => self.activate_gesture(ui, mode, style).await,
            None => self.activate_panel(ui, style).await,
        }
    }

    /// Gesture activation: resolve the focused window's actionable elements,
    /// show the hint overlay (via the UI thread), and dispatch `mode` on
    /// whatever the user selects.
    async fn activate_gesture(
        &self,
        ui: &UiHandle,
        mode: Mode,
        style: HintStyle,
    ) -> Result<Activation> {
        let (geom, located) = self.located_elements().await?;
        let hints = hints::assign(&located, style);
        if hints.is_empty() {
            return Ok(Activation::NoElements);
        }

        let Some(hint) = ui.select_hint(geom.clone(), hints).await? else {
            return Ok(Activation::Cancelled);
        };

        let how = self.dispatch(&geom, &hint, mode).await?;
        Ok(Activation::Activated {
            element: hint.element,
            how,
        })
    }

    /// While the panel is open, scrolling invalidates element positions; the
    /// panel asks for a re-scan over `refetch` when the user releases Ctrl.
    async fn activate_panel(&self, ui: &UiHandle, style: HintStyle) -> Result<Activation> {
        let geom = {
            let _p = Phase::start("niri");
            niri::focused_window_geometry().context("failed to resolve focused-window geometry")?
        };
        // Resolve the a11y frame once, while the app still holds focus: the
        // panel takes exclusive keyboard focus once mapped, so the app loses
        // AT-SPI `State::Active` and an active-frame search during a re-scan
        // would always come up empty.
        let frame = match tree::active_frame(&self.a11y).await {
            Ok(Some(frame)) => Some(frame),
            Ok(None) => {
                tracing::info!("panel: no active a11y frame; opening empty");
                None
            }
            Err(e) => {
                tracing::info!(error = %e, "panel: a11y unavailable; opening empty");
                None
            }
        };
        let hints = hints::assign(&self.located_or_empty(&geom, frame.clone()).await, style);

        let (refetch_tx, mut refetch_rx) = tokio::sync::mpsc::channel::<crate::ui::HintsReply>(1);
        let panel = ui.run_panel(geom.clone(), hints, refetch_tx);
        tokio::pin!(panel);
        let mut refetch_open = true;
        let outcome = loop {
            tokio::select! {
                outcome = &mut panel => break outcome?,
                request = refetch_rx.recv(), if refetch_open => match request {
                    Some(reply) => {
                        let located = self.located_or_empty(&geom, frame.clone()).await;
                        let _ = reply.send(hints::assign(&located, style));
                    }
                    // Sender dropped: the panel is tearing down.
                    None => refetch_open = false,
                },
            }
        };
        Ok(Activation::Panel(outcome))
    }

    /// `located_in_frame`, degrading a missing frame or a11y failure to
    /// "no elements" (panel mode).
    async fn located_or_empty(
        &self,
        geom: &WindowGeometry,
        frame: Option<atspi::ObjectRefOwned>,
    ) -> Vec<(Element, PhysicalRect)> {
        let Some(frame) = frame else {
            return Vec::new();
        };
        match self.located_in_frame(geom, frame).await {
            Ok(located) => located,
            Err(e) => {
                tracing::info!(error = %e, "panel: no accessible elements");
                Vec::new()
            }
        }
    }

    /// Dispatch `mode` on the selected hint.
    ///
    /// [`Mode::LeftClick`] prefers the element's semantic AT-SPI `Action` verb
    /// (no cursor warp, hits the real target) and falls back to a synthetic
    /// left click. Every other mode is an inherently pointer-level gesture
    /// (right/middle/double click, or a bare warp) with no AT-SPI equivalent,
    /// so it goes straight to the synthetic pointer.
    ///
    /// The caller must have already unmapped the overlay (`overlay::select`
    /// does), so the synthetic gesture lands on the real application surface.
    pub async fn dispatch(
        &self,
        geom: &WindowGeometry,
        hint: &Hint,
        mode: Mode,
    ) -> Result<Dispatched> {
        let _p = Phase::start("dispatch");

        // Primary path: the element's own AT-SPI Action verb.
        if mode == Mode::LeftClick {
            if let Some(verb) = action::try_action(&self.a11y, &hint.element).await? {
                return Ok(Dispatched::Action(verb));
            }
        }

        // Fallback / non-left modes: synthesize the gesture at the rect centre.
        let (cx, cy) = hint.rect.center();
        pointer::dispatch_at(geom, cx, cy, mode)?;
        Ok(Dispatched::Pointer { mode, at: (cx, cy) })
    }
}

/// How an activation was carried out (for reporting).
#[derive(Debug)]
pub enum Dispatched {
    /// AT-SPI `Action.DoAction` on the named verb.
    Action(String),
    /// Synthetic pointer `mode` at output-local physical `(x, y)`.
    Pointer { mode: Mode, at: (i32, i32) },
}

impl fmt::Display for Dispatched {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Dispatched::Action(verb) => write!(f, "AT-SPI action {verb:?}"),
            Dispatched::Pointer { mode, at: (x, y) } => {
                write!(f, "synthetic {mode} at ({x}, {y})")
            }
        }
    }
}

/// How a panel interaction ended.
#[derive(Debug)]
pub enum PanelOutcome {
    /// Dismissed without acting on an element (scrolling/warping may still
    /// have happened).
    Dismissed { scrolls: u32, warped: bool },
    /// Left-clicked (possibly double-clicked) the selected element.
    Clicked {
        element: Element,
        at: (i32, i32),
        double: bool,
    },
    /// Right-clicked the selected element.
    RightClicked { element: Element, at: (i32, i32) },
}

impl fmt::Display for PanelOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PanelOutcome::Dismissed { scrolls, warped } => {
                write!(f, "panel dismissed (scrolls: {scrolls}, warped: {warped})")
            }
            PanelOutcome::Clicked {
                element,
                at: (x, y),
                double,
            } => write!(
                f,
                "panel: {} {:?} {:?} at ({x}, {y})",
                if *double { "double-clicked" } else { "clicked" },
                element.role,
                element.name,
            ),
            PanelOutcome::RightClicked {
                element,
                at: (x, y),
            } => write!(
                f,
                "panel: right-clicked {:?} {:?} at ({x}, {y})",
                element.role, element.name,
            ),
        }
    }
}

/// The outcome of one [`Session::activate`], renderable as a single status line
/// (printed by `oneshot`, sent over the socket by the daemon).
#[derive(Debug)]
pub enum Activation {
    /// The focused window exposed no actionable elements.
    NoElements,
    /// The user dismissed the overlay (`Esc`) without selecting.
    Cancelled,
    /// A target was selected and dispatched.
    Activated { element: Element, how: Dispatched },
    /// A panel interaction finished.
    Panel(PanelOutcome),
}

impl fmt::Display for Activation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Activation::NoElements => write!(f, "no actionable elements found"),
            Activation::Cancelled => write!(f, "cancelled"),
            Activation::Activated { element, how } => write!(
                f,
                "selected {:?} {:?} ({}); dispatched: {how}",
                element.role,
                element.name,
                element.object.path()
            ),
            Activation::Panel(outcome) => outcome.fmt(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;

    /// The socket protocol is the `Request` value-enum names: the new `panel`
    /// line parses, legacy gesture lines keep their meaning, and garbage is
    /// rejected (the daemon then falls back to the default).
    #[test]
    fn request_parses_socket_lines() {
        assert_eq!(Request::from_str("panel", false), Ok(Request::Panel));
        assert_eq!(
            Request::from_str("left_click", false),
            Ok(Request::LeftClick)
        );
        assert_eq!(
            Request::from_str("double_click", false),
            Ok(Request::DoubleClick)
        );
        assert_eq!(Request::from_str("warp", false), Ok(Request::Warp));
        assert!(Request::from_str("does-not-exist", false).is_err());
    }

    #[test]
    fn request_round_trips_through_display() {
        for r in Request::value_variants() {
            assert_eq!(Request::from_str(&r.to_string(), false), Ok(*r));
        }
    }
}
