pub mod hints_window;
pub mod panel;

use std::future::Future;

use anyhow::{anyhow, Context, Result};
use gtk::glib;
use gtk::prelude::*;
use tokio::sync::{mpsc, oneshot};

use crate::hints::Hint;
use crate::niri::WindowGeometry;
use crate::session::PanelOutcome;

/// Reply slot for a panel-initiated re-scan of the focused window (the
/// element positions go stale once the user scrolls).
pub type HintsReply = oneshot::Sender<Vec<Hint>>;

/// A request from the worker thread to the UI thread.
enum UiRequest {
    /// Show the hint overlay and resolve the user's selection.
    SelectHint {
        geom: WindowGeometry,
        hints: Vec<Hint>,
        reply: oneshot::Sender<Result<Option<Hint>>>,
    },
    /// Run a full panel interaction (hints overlay + panel window).
    Panel {
        geom: WindowGeometry,
        hints: Vec<Hint>,
        /// Lets the panel ask the worker to re-scan after scrolling.
        refetch: mpsc::Sender<HintsReply>,
        reply: oneshot::Sender<Result<PanelOutcome>>,
    },
}

/// Worker-side handle to the UI thread.
#[derive(Clone)]
pub struct UiHandle {
    tx: mpsc::UnboundedSender<UiRequest>,
}

impl UiHandle {
    /// Show hint labels and wait for the user to select one (`None` = Esc).
    pub async fn select_hint(
        &self,
        geom: WindowGeometry,
        hints: Vec<Hint>,
    ) -> Result<Option<Hint>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(UiRequest::SelectHint { geom, hints, reply })
            .map_err(|_| anyhow!("UI thread is gone"))?;
        rx.await
            .map_err(|_| anyhow!("UI thread dropped the request"))?
    }

    /// Run a panel interaction and wait for its outcome. Re-scan requests
    /// arriving on the paired receiver must be serviced while waiting.
    pub async fn run_panel(
        &self,
        geom: WindowGeometry,
        hints: Vec<Hint>,
        refetch: mpsc::Sender<HintsReply>,
    ) -> Result<PanelOutcome> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(UiRequest::Panel {
                geom,
                hints,
                refetch,
                reply,
            })
            .map_err(|_| anyhow!("UI thread is gone"))?;
        rx.await
            .map_err(|_| anyhow!("UI thread dropped the request"))?
    }
}

/// Initialise GTK on the current thread, run `work` on a tokio worker thread,
/// and pump the GLib main loop until the work completes.
pub fn run<F, Fut>(work: F) -> Result<()>
where
    F: FnOnce(UiHandle) -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>>,
{
    // Keep peck's own windows out of the AT-SPI tree it scrapes.
    std::env::set_var("GTK_A11Y", "none");
    if std::env::var_os("GSK_RENDERER").is_none() {
        std::env::set_var("GSK_RENDERER", "cairo");
    }
    gtk::init().context("failed to initialise GTK (is a Wayland session running?)")?;

    let (tx, rx) = mpsc::unbounded_channel::<UiRequest>();
    let main_loop = glib::MainLoop::new(None, false);

    let worker = std::thread::Builder::new()
        .name("peck-worker".into())
        .spawn({
            let main_loop = main_loop.clone();
            move || {
                // Quit the GLib loop when the work finishes — and also on
                // panic, so the process never hangs with a dead worker.
                struct Quit(glib::MainLoop);
                impl Drop for Quit {
                    fn drop(&mut self) {
                        self.0.quit();
                    }
                }
                let _quit = Quit(main_loop);

                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("failed to build the tokio runtime")?;
                rt.block_on(work(UiHandle { tx }))
            }
        })
        .context("failed to spawn the worker thread")?;

    glib::spawn_future_local(serve(rx));
    main_loop.run();

    worker
        .join()
        .map_err(|_| anyhow!("worker thread panicked"))?
}

/// Service UI requests on the GTK thread, one at a time (the daemon already
/// refuses overlapping activations).
async fn serve(mut rx: mpsc::UnboundedReceiver<UiRequest>) {
    while let Some(request) = rx.recv().await {
        match request {
            UiRequest::SelectHint { geom, hints, reply } => {
                let result = hints_window::select(&geom, hints).await;
                let _ = reply.send(result);
            }
            UiRequest::Panel {
                geom,
                hints,
                refetch,
                reply,
            } => {
                let result = panel::run(&geom, hints, refetch).await;
                let _ = reply.send(result);
            }
        }
    }
}

/// Make a window click-through: pointer events pass to the surface beneath.
/// Keyboard interactivity is a separate layer-shell property and is unaffected.
pub(crate) fn click_through(window: &gtk::Window) {
    window.connect_realize(|w| {
        if let Some(surface) = w.surface() {
            surface.set_input_region(Some(&gtk::cairo::Region::create()));
        }
    });
}

/// Intersection area of two `(x, y, w, h)` logical-px rects.
pub(crate) fn overlap_area(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> f64 {
    let w = (a.0 + a.2).min(b.0 + b.2) - a.0.max(b.0);
    let h = (a.1 + a.3).min(b.1 + b.3) - a.1.max(b.1);
    if w > 0.0 && h > 0.0 {
        w * h
    } else {
        0.0
    }
}

/// Install peck's CSS once per process.
pub(crate) fn ensure_css() {
    thread_local! {
        static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if DONE.with(|d| d.replace(true)) {
        return;
    }
    let css = gtk::CssProvider::new();
    css.load_from_string(include_str!("style.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
