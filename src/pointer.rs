//! Synthetic pointer click via `zwlr_virtual_pointer_v1`.

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
    Connection, Proxy, QueueHandle,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

use crate::instrument::Phase;
use crate::niri::WindowGeometry;

/// Linux input event codes for the mouse buttons.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Mode {
    /// Synthesize a left click (default). Prefers the element's semantic
    /// AT-SPI action when one is available; this is the only mode that does.
    #[default]
    #[value(name = "left_click")]
    LeftClick,
    /// Synthesize a right click (e.g. to open a context menu).
    #[value(name = "right_click")]
    RightClick,
    /// Synthesize a middle click.
    #[value(name = "middle_click")]
    MiddleClick,
    /// Synthesize a double left click.
    #[value(name = "double_click")]
    DoubleClick,
    /// Warp the cursor to the target without clicking.
    #[value(name = "warp")]
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

/// Synthesize the gesture for `mode` at output-local physical `(x, y)` on the
/// focused window's output.
pub fn dispatch_at(geom: &WindowGeometry, x: i32, y: i32, mode: Mode) -> Result<()> {
    let _p = Phase::start("dispatch-pointer");

    let conn = Connection::connect_to_env().context("failed to connect to the Wayland display")?;
    let (globals, mut event_queue) =
        registry_queue_init(&conn).context("failed to init Wayland registry")?;
    let qh = event_queue.handle();

    let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).map_err(|e| {
        anyhow!("zwlr_virtual_pointer_manager_v1 unavailable (cannot synthesize click): {e}")
    })?;

    let mut state = Pointer {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
    };

    event_queue
        .roundtrip(&mut state)
        .context("Wayland roundtrip for outputs/seats failed")?;

    let output = state
        .output_state
        .outputs()
        .find(|o| {
            state.output_state.info(o).and_then(|i| i.name).as_deref() == Some(&geom.output_name)
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
    let pointer = manager.create_virtual_pointer_with_output(Some(&seat), Some(&output), &qh, ());

    let (xe, ye) = (geom.output_mode.0 as u32, geom.output_mode.1 as u32);
    let xc = x.clamp(0, xe.saturating_sub(1) as i32) as u32;
    let yc = y.clamp(0, ye.saturating_sub(1) as i32) as u32;

    // Warp first; then synthesize the click(s).
    pointer.motion_absolute(0, xc, yc, xe, ye);
    pointer.frame();
    match mode {
        Mode::Warp => {}
        Mode::LeftClick => click(&pointer, BTN_LEFT, 1),
        Mode::RightClick => click(&pointer, BTN_RIGHT, 1),
        Mode::MiddleClick => click(&pointer, BTN_MIDDLE, 1),
        Mode::DoubleClick => {
            click(&pointer, BTN_LEFT, 1);
            click(&pointer, BTN_LEFT, 3);
        }
    }
    pointer.destroy();

    // Flush the requests to the compositor
    event_queue
        .roundtrip(&mut state)
        .context("Wayland roundtrip to flush the synthetic click failed")?;

    Ok(())
}

/// Press and release `button` as one click
fn click(pointer: &ZwlrVirtualPointerV1, button: u32, time: u32) {
    pointer.button(time, button, wl_pointer::ButtonState::Pressed);
    pointer.frame();
    pointer.button(time + 1, button, wl_pointer::ButtonState::Released);
    pointer.frame();
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
