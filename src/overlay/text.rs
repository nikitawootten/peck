use std::collections::HashMap;

use anyhow::{anyhow, Result};
use fontdue::{Font, FontSettings, Metrics};

struct Glyph {
    metrics: Metrics,
    /// Coverage bitmap, `metrics.width * metrics.height`, row-major.
    coverage: Vec<u8>,
}

pub struct GlyphAtlas {
    glyphs: HashMap<char, Glyph>,
    px: f32,
    advance: usize,
}

impl GlyphAtlas {
    /// Rasterize the hint alphabet (the characters in `chars`) at `px`, using a
    /// system-provided monospace font.
    pub fn new(px: f32, chars: impl IntoIterator<Item = char>) -> Result<Self> {
        let font = system_monospace(px)?;

        let mut glyphs = HashMap::new();
        let mut advance = 0usize;
        for c in chars {
            let (metrics, coverage) = font.rasterize(c, px);
            advance = advance.max(metrics.advance_width.ceil() as usize);
            glyphs.insert(c, Glyph { metrics, coverage });
        }
        Ok(Self {
            glyphs,
            px,
            advance,
        })
    }

    /// Pixel size of the rendered text box for `text` (width, height).
    pub fn measure(&self, text: &str) -> (usize, usize) {
        (self.advance * text.chars().count(), self.px.ceil() as usize)
    }

    /// Blit `text` with its top-left at (`x`, `y`), `color` foreground over the
    /// existing (assumed opaque) background, via `put`.
    pub fn draw(
        &self,
        text: &str,
        x: i32,
        y: i32,
        color: [u8; 4],
        bg: [u8; 4],
        mut put: impl FnMut(i32, i32, [u8; 4]),
    ) {
        // Baseline placed so the cap height sits within the px box.
        let baseline = y + (self.px * 0.78).round() as i32;
        let mut pen = x;
        for c in text.chars() {
            if let Some(g) = self.glyphs.get(&c) {
                let gx = pen + g.metrics.xmin;
                let gy = baseline - g.metrics.height as i32 - g.metrics.ymin;
                for row in 0..g.metrics.height {
                    for col in 0..g.metrics.width {
                        let cov = g.coverage[row * g.metrics.width + col];
                        if cov == 0 {
                            continue;
                        }
                        put(gx + col as i32, gy + row as i32, blend(bg, color, cov));
                    }
                }
            }
            pen += self.advance as i32;
        }
    }
}

fn system_monospace(px: f32) -> Result<Font> {
    /// Well-known monospace families, tried in order if the generic `monospace`
    /// family does not resolve.
    const FALLBACK_FAMILIES: &[&str] = &[
        "DejaVu Sans Mono",
        "Liberation Mono",
        "Noto Sans Mono",
        "Source Code Pro",
        "JetBrains Mono",
        "Hack",
        "Ubuntu Mono",
    ];

    let mut db = fontdb::Database::new();
    db.load_system_fonts();

    // Preference-ordered candidate faces.
    let mut ids = Vec::new();
    let by_name = std::iter::once(fontdb::Family::Monospace)
        .chain(FALLBACK_FAMILIES.iter().map(|n| fontdb::Family::Name(n)));
    for family in by_name {
        let query = fontdb::Query {
            families: &[family],
            ..Default::default()
        };
        ids.extend(db.query(&query));
    }
    ids.extend(db.faces().filter(|f| f.monospaced).map(|f| f.id));

    for id in ids {
        let font = db.with_face_data(id, |data, index| {
            Font::from_bytes(
                data,
                FontSettings {
                    collection_index: index,
                    scale: px,
                    ..Default::default()
                },
            )
            .ok()
        });
        if let Some(Some(font)) = font {
            return Ok(font);
        }
    }

    Err(anyhow!(
        "no usable system monospace font found; install one (e.g. DejaVu Sans Mono)"
    ))
}

fn blend(bg: [u8; 4], fg: [u8; 4], cov: u8) -> [u8; 4] {
    let a = cov as u16;
    let ia = 255 - a;
    let mix = |b: u8, f: u8| (((b as u16 * ia) + (f as u16 * a)) / 255) as u8;
    [
        mix(bg[0], fg[0]),
        mix(bg[1], fg[1]),
        mix(bg[2], fg[2]),
        0xFF,
    ]
}
