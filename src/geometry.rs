use crate::niri::WindowGeometry;

/// A rectangle in output-local physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl PhysicalRect {
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

/// Transform an element's window-relative logical extents `(x, y, w, h)` into
/// output-local physical pixels for the given focused-window geometry.
pub fn correct(window_rel: (i32, i32, i32, i32), geom: &WindowGeometry) -> PhysicalRect {
    let (ex, ey, ew, eh) = window_rel;
    let (ox, oy) = geom.content_origin;
    let s = geom.scale;

    // Window-relative logical → output-local logical → output-local physical.
    let lx = ox + ex as f64;
    let ly = oy + ey as f64;

    PhysicalRect {
        x: (lx * s).round() as i32,
        y: (ly * s).round() as i32,
        w: (ew as f64 * s).round() as i32,
        h: (eh as f64 * s).round() as i32,
    }
}
