//! Synthetic pointer gestures (warp/click/scroll) via `zwlr_virtual_pointer_v1`.

use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use smithay_client_toolkit::{
    delegate_output, delegate_registry, delegate_seat,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{Capability, SeatHandler, SeatState},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_pointer, wl_seat},
    Connection, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

use crate::instrument::Phase;
use crate::niri::WindowGeometry;

/// Linux input event codes for the mouse buttons.
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

/// A pointer gesture peck can synthesize on a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Synthesize a left click (default). Prefers the element's semantic
    /// AT-SPI action when one is available; this is the only mode that does.
    #[default]
    LeftClick,
    /// Synthesize a right click (e.g. to open a context menu).
    RightClick,
    /// Synthesize a middle click.
    MiddleClick,
    /// Synthesize a double left click.
    DoubleClick,
    /// Warp the cursor to the target without clicking.
    Warp,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::LeftClick => "left_click",
            Mode::RightClick => "right_click",
            Mode::MiddleClick => "middle_click",
            Mode::DoubleClick => "double_click",
            Mode::Warp => "warp",
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A live virtual pointer on the focused window's output.
///
/// Holds its own Wayland connection so gestures can be synthesized repeatedly
/// (panel mode: clicks, warps, scrolling) without per-gesture setup cost.
pub struct VirtualPointer {
    queue: EventQueue<Pointer>,
    state: Pointer,
    pointer: ZwlrVirtualPointerV1,
    /// Output physical mode size, the coordinate space of `motion_absolute`.
    extent: (u32, u32),
    /// Epoch for the millisecond timestamps input events carry.
    epoch: Instant,
}

impl VirtualPointer {
    /// Connect and create a virtual pointer targeting `geom`'s output.
    pub fn new(geom: &WindowGeometry) -> Result<Self> {
        let _p = Phase::start("virtual-pointer");

        let conn =
            Connection::connect_to_env().context("failed to connect to the Wayland display")?;
        let (globals, mut queue) =
            registry_queue_init(&conn).context("failed to init Wayland registry")?;
        let qh = queue.handle();

        let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).map_err(|e| {
            anyhow!("zwlr_virtual_pointer_manager_v1 unavailable (cannot synthesize input): {e}")
        })?;

        let mut state = Pointer {
            registry_state: RegistryState::new(&globals),
            output_state: OutputState::new(&globals, &qh),
            seat_state: SeatState::new(&globals, &qh),
        };

        queue
            .roundtrip(&mut state)
            .context("Wayland roundtrip for outputs/seats failed")?;

        let output = state
            .output_state
            .outputs()
            .find(|o| {
                state.output_state.info(o).and_then(|i| i.name).as_deref()
                    == Some(&geom.output_name)
            })
            .ok_or_else(|| {
                anyhow!(
                    "focused output {:?} not found via Wayland",
                    geom.output_name
                )
            })?;

        let seat = state
            .seat_state
            .seats()
            .next()
            .context("no Wayland seat available for the virtual pointer")?;

        if manager.version() < 2 {
            return Err(anyhow!(
                "zwlr_virtual_pointer_manager_v1 is only v{} here; v2 \
                 (create_virtual_pointer_with_output) is required to target an output",
                manager.version()
            ));
        }
        let pointer =
            manager.create_virtual_pointer_with_output(Some(&seat), Some(&output), &qh, ());

        Ok(Self {
            queue,
            state,
            pointer,
            extent: (u32::from(geom.output_mode.0), u32::from(geom.output_mode.1)),
            epoch: Instant::now(),
        })
    }

    fn now_ms(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }

    fn flush(&self) -> Result<()> {
        self.queue
            .flush()
            .context("failed to flush virtual-pointer requests")
    }

    /// Move the cursor to output-local physical `(x, y)`, clamped on-output.
    pub fn warp(&self, x: i32, y: i32) -> Result<()> {
        let (xe, ye) = self.extent;
        let xc = x.clamp(0, xe.saturating_sub(1) as i32) as u32;
        let yc = y.clamp(0, ye.saturating_sub(1) as i32) as u32;
        self.pointer.motion_absolute(self.now_ms(), xc, yc, xe, ye);
        self.pointer.frame();
        self.flush()
    }

    /// Press and release `button` as one click.
    pub fn click(&self, button: u32) -> Result<()> {
        let t = self.now_ms();
        self.pointer
            .button(t, button, wl_pointer::ButtonState::Pressed);
        self.pointer.frame();
        self.pointer
            .button(t + 1, button, wl_pointer::ButtonState::Released);
        self.pointer.frame();
        self.flush()
    }

    /// Scroll by `value` along `axis` (positive = down/right; ~15.0 is one
    /// wheel detent), delivered to whatever surface is under the cursor.
    pub fn scroll(&self, axis: wl_pointer::Axis, value: f64) -> Result<()> {
        self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
        self.pointer.axis(self.now_ms(), axis, value);
        self.pointer.frame();
        self.flush()
    }

    /// Block until the compositor has processed everything sent so far.
    pub fn roundtrip(&mut self) -> Result<()> {
        self.queue
            .roundtrip(&mut self.state)
            .context("Wayland roundtrip for the virtual pointer failed")?;
        Ok(())
    }
}

impl Drop for VirtualPointer {
    fn drop(&mut self) {
        self.pointer.destroy();
        let _ = self.queue.roundtrip(&mut self.state);
    }
}

/// Synthesize the gesture for `mode` at output-local physical `(x, y)` on the
/// focused window's output.
pub fn dispatch_at(geom: &WindowGeometry, x: i32, y: i32, mode: Mode) -> Result<()> {
    let _p = Phase::start("dispatch-pointer");

    let mut vp = VirtualPointer::new(geom)?;

    // Warp first; then synthesize the click(s).
    vp.warp(x, y)?;
    match mode {
        Mode::Warp => {}
        Mode::LeftClick => vp.click(BTN_LEFT)?,
        Mode::RightClick => vp.click(BTN_RIGHT)?,
        Mode::MiddleClick => vp.click(BTN_MIDDLE)?,
        Mode::DoubleClick => {
            vp.click(BTN_LEFT)?;
            vp.click(BTN_LEFT)?;
        }
    }

    // Make sure the compositor has processed the gesture before returning.
    vp.roundtrip()
}

struct Pointer {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
}

impl OutputHandler for Pointer {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl SeatHandler for Pointer {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _: Capability,
    ) {
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _: Capability,
    ) {
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl ProvidesRegistryState for Pointer {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_output!(Pointer);
delegate_seat!(Pointer);
delegate_registry!(Pointer);
wayland_client::delegate_noop!(Pointer: ignore ZwlrVirtualPointerManagerV1);
wayland_client::delegate_noop!(Pointer: ignore ZwlrVirtualPointerV1);
