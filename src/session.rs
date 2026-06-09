use anyhow::{Context, Result};
use atspi::AccessibilityConnection;

use std::fmt;

use crate::atspi::{action, extents, tree, Element};
use crate::geometry::{self, PhysicalRect};
use crate::hints::{self, Hint};
use crate::instrument::Phase;
use crate::niri::{self, WindowGeometry};
use crate::overlay;
use crate::pointer::{self, Mode};

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
        let frame = tree::active_frame(&self.a11y)
            .await?
            .context("no active toplevel frame found (is a window focused, and a11y enabled?)")?;

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

        let geom = {
            let _p = Phase::start("niri");
            niri::focused_window_geometry().context("failed to resolve focused-window geometry")?
        };

        let _p = Phase::start("extents");
        let mut located = Vec::with_capacity(elements.len());
        for el in elements {
            match extents::window_extents(&self.a11y, &el.object).await {
                Ok((x, y, w, h)) => {
                    let rect = geometry::correct((x - fx, y - fy, w, h), &geom);
                    located.push((el, rect));
                }
                Err(e) => {
                    tracing::debug!(name = %el.name, error = %e, "skipping element without extents");
                }
            }
        }

        Ok((geom, located))
    }

    /// Run one full activation: resolve the focused window's actionable
    /// elements, show the hint overlay, and dispatch whatever the user selects.
    ///
    /// This is the single pipeline shared by `oneshot` and the daemon — the
    /// daemon simply calls it once per `peck activate` request, reusing the warm
    /// AT-SPI connection this `Session` already holds. `mode` selects the
    /// gesture dispatched on the chosen target.
    pub async fn activate(&self, mode: Mode) -> Result<Activation> {
        let (geom, located) = self.located_elements().await?;
        let hints = hints::assign(&located);
        if hints.is_empty() {
            return Ok(Activation::NoElements);
        }

        let Some(hint) = overlay::select(&geom, &hints)? else {
            return Ok(Activation::Cancelled);
        };

        let how = self.dispatch(&geom, &hint, mode).await?;
        Ok(Activation::Activated {
            element: hint.element,
            how,
        })
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
        }
    }
}
