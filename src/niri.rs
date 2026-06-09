use anyhow::{anyhow, Context, Result};
use niri_ipc::socket::Socket;
use niri_ipc::{Request, Response, Window};

/// Resolved geometry for the focused window: everything needed to map
/// AT-SPI window-relative extents to physical pixels.
#[derive(Debug, Clone)]
pub struct WindowGeometry {
    pub window_id: u64,
    pub app_id: Option<String>,
    /// Output the window is on.
    pub output_name: String,
    /// Output logical origin (compositor logical coords).
    pub output_logical_origin: (i32, i32),
    /// Output physical mode size (for the virtual pointer / overlay sizing).
    pub output_mode: (u16, u16),
    /// Output scale factor (logical → physical).
    pub scale: f64,
    /// Window Wayland-surface top-left in **output-local logical px**.
    pub content_origin: (f64, f64),
    /// Window Wayland-surface size in logical px.
    pub window_size: (i32, i32),
}

fn request(req: Request) -> Result<Response> {
    Socket::connect()
        .context("failed to connect to niri socket ($NIRI_SOCKET)")?
        .send(req)
        .context("niri IPC send failed")?
        .map_err(|e| anyhow!("niri returned an error: {e}"))
}

/// Read the configured `gaps` value. niri-ipc does not expose it, so parse
/// the niri config.
fn configured_gap() -> f64 {
    const DEFAULT_GAP: f64 = 16.0;

    for path in config_candidates() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Some(gap) = parse_gaps(&text) {
                return gap;
            }
        }
    }

    tracing::warn!(
        default = DEFAULT_GAP,
        "could not read `gaps` from the niri config; positions may be off in \
         columns >1. Is $NIRI_SOCKET valid?"
    );
    DEFAULT_GAP
}

/// Candidate niri config paths, in priority order.
fn config_candidates() -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let mut paths = Vec::new();
    if let Some(p) = std::env::var_os("NIRI_CONFIG") {
        paths.push(PathBuf::from(p));
    }
    if let Some(p) = niri_config_from_process() {
        paths.push(p);
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        paths.push(PathBuf::from(xdg).join("niri/config.kdl"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home).join(".config/niri/config.kdl"));
    }
    paths.push(PathBuf::from("/etc/niri/config.kdl"));
    paths
}

/// Find the config path from the running niri's process environment.
///
/// `$NIRI_SOCKET` (which peck already relies on for IPC) is named
/// `niri.<wayland-display>.<pid>.sock`, so niri's PID is extracted from
/// the socket name and `NIRI_CONFIG` is read.
fn niri_config_from_process() -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    let sock = std::env::var_os("NIRI_SOCKET")?;
    let base = std::path::Path::new(&sock).file_name()?.to_str()?;

    for part in base.split('.') {
        let Ok(pid) = part.parse::<u32>() else {
            continue;
        };
        // Confirm this PID is actually niri before trusting its environment.
        match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
            Ok(comm) if comm.trim() == "niri" => {}
            _ => continue,
        }
        let Ok(environ) = std::fs::read(format!("/proc/{pid}/environ")) else {
            continue;
        };
        for kv in environ.split(|&b| b == 0) {
            if let Some(rest) = kv.strip_prefix(b"NIRI_CONFIG=") {
                return Some(PathBuf::from(std::ffi::OsStr::from_bytes(rest)));
            }
        }
    }
    None
}

/// Extract the `gaps` value from KDL text
fn parse_gaps(text: &str) -> Option<f64> {
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line
            .strip_prefix("\"gaps\"")
            .or_else(|| line.strip_prefix("gaps"))
        else {
            continue;
        };
        // Guard against matching a longer node name like `gaps-foo`.
        if rest.starts_with(|c: char| c.is_whitespace()) {
            if let Some(tok) = rest.split_whitespace().next() {
                if let Ok(v) = tok.trim_matches('"').parse::<f64>() {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Column width = the (shared) tile width of any tile in that column.
fn column_width(windows: &[Window], col: usize) -> f64 {
    windows
        .iter()
        .filter(|w| w.layout.pos_in_scrolling_layout.map(|(c, _)| c) == Some(col))
        .map(|w| w.layout.tile_size.0)
        .fold(0.0, f64::max)
}

/// Resolve the focused window's geometry.
pub fn focused_window_geometry() -> Result<WindowGeometry> {
    let window = match request(Request::FocusedWindow)? {
        Response::FocusedWindow(Some(w)) => w,
        Response::FocusedWindow(None) => {
            return Err(anyhow!(
                "no focused window (a layer-shell surface may hold focus)"
            ))
        }
        other => {
            return Err(anyhow!(
                "unexpected niri response to FocusedWindow: {other:?}"
            ))
        }
    };

    let output = match request(Request::FocusedOutput)? {
        Response::FocusedOutput(Some(o)) => o,
        Response::FocusedOutput(None) => return Err(anyhow!("focused window has no output")),
        other => {
            return Err(anyhow!(
                "unexpected niri response to FocusedOutput: {other:?}"
            ))
        }
    };

    let gap = configured_gap();
    let content_origin = content_origin(&window, gap)?;

    let logical = output
        .logical
        .ok_or_else(|| anyhow!("focused output has no logical geometry (disabled?)"))?;
    let mode = output
        .current_mode
        .and_then(|i| output.modes.get(i))
        .map(|m| (m.width, m.height))
        .ok_or_else(|| anyhow!("focused output has no current mode"))?;

    Ok(WindowGeometry {
        window_id: window.id,
        app_id: window.app_id.clone(),
        output_name: output.name.clone(),
        output_logical_origin: (logical.x, logical.y),
        output_mode: mode,
        scale: logical.scale,
        content_origin,
        window_size: window.layout.window_size,
    })
}

/// Compute the window's Wayland-surface top-left in output-local logical px.
fn content_origin(window: &Window, gap: f64) -> Result<(f64, f64)> {
    let l = &window.layout;
    let (off_x, off_y) = l.window_offset_in_tile;

    // Floating windows carry their tile position directly.
    if window.is_floating {
        let (tx, ty) = l
            .tile_pos_in_workspace_view
            .ok_or_else(|| anyhow!("floating window missing tile_pos_in_workspace_view"))?;
        return Ok((tx + off_x, ty + off_y));
    }

    // Tiled: reconstruct from the scrolling layout.
    let (col, tile_idx) = l
        .pos_in_scrolling_layout
        .ok_or_else(|| anyhow!("tiled window missing pos_in_scrolling_layout"))?;

    let scrolling_view_pos = focused_workspace_view_pos(window.workspace_id)?;

    // All windows on the focused output, to sum column widths / tile heights.
    let windows = match request(Request::Windows)? {
        Response::Windows(ws) => ws,
        other => return Err(anyhow!("unexpected niri response to Windows: {other:?}")),
    };
    let same_ws: Vec<Window> = windows
        .into_iter()
        .filter(|w| w.workspace_id == window.workspace_id)
        .collect();

    // x: strip_x(col) - scrolling_view_pos. Column 1's left edge is at x=0.
    let strip_x: f64 = (1..col).map(|c| column_width(&same_ws, c) + gap).sum();
    let tile_x = strip_x - scrolling_view_pos;

    // y: top-aligned column. Leading gap + stacked tiles above this one.
    let working_area_top = 0.0; // no top strut in this setup
    let tiles_above: f64 = same_ws
        .iter()
        .filter(|w| {
            w.layout
                .pos_in_scrolling_layout
                .is_some_and(|(c, t)| c == col && t < tile_idx)
        })
        .map(|w| w.layout.tile_size.1 + gap)
        .sum();
    let tile_y = working_area_top + gap + tiles_above;

    Ok((tile_x + off_x, tile_y + off_y))
}

/// `scrolling_view_pos` for the workspace the window is on.
fn focused_workspace_view_pos(workspace_id: Option<u64>) -> Result<f64> {
    let workspaces = match request(Request::Workspaces)? {
        Response::Workspaces(ws) => ws,
        other => return Err(anyhow!("unexpected niri response to Workspaces: {other:?}")),
    };
    workspaces
        .iter()
        .find(|w| Some(w.id) == workspace_id)
        .map(|w| w.scrolling_view_pos)
        .ok_or_else(|| anyhow!("could not find the window's workspace"))
}
