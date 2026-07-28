//! Cairo/Pango renderer for a [`Snapshot`].
//!
//! libghostty-vt owns terminal state; drawing is ours. This is the "separate
//! native renderer" half of the composition model — see
//! `docs/spikes/terminal-embedding.md` for why the GPU renderer inside Ghostty
//! is not reachable from a Linux embedder.

use ansible_terminal::{CellWidth, CursorShape, Rgb, Snapshot};
use cairo::Context;
use pango::FontDescription;

/// Monospace metrics, measured once from the font.
///
/// Both fields are finite and at least `1.0` — [`Renderer::metrics`] clamps them
/// — which is what makes [`pixel_size`](CellMetrics::pixel_size) exact.
#[derive(Debug, Clone, Copy)]
pub struct CellMetrics {
    pub width: f64,
    pub height: f64,
}

impl CellMetrics {
    /// Cell size as whole pixels, for the PTY winsize the child is told about.
    pub fn pixel_size(self) -> (u32, u32) {
        (whole_pixels(self.width), whole_pixels(self.height))
    }
}

/// A finite, non-negative pixel measurement as a `u32`.
///
/// Font metrics come from Pango as `f64`. Clamping before the conversion is what
/// makes it exact rather than implementation-defined: without it, a NaN or an
/// out-of-range value would convert to whatever saturation happens to give.
fn whole_pixels(v: f64) -> u32 {
    let clamped = v.clamp(0.0, f64::from(u32::MAX)).trunc();
    // Integral and inside `0..=u32::MAX` by the clamp above, so this is exact.
    // (`clamp` propagates NaN, which `as` then maps to 0 — also in range.)
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pixels = clamped as u32;
    pixels
}

pub struct Renderer {
    font: FontDescription,
    metrics: Option<CellMetrics>,
}

impl Renderer {
    pub fn new(family: &str, size_pt: f64) -> Self {
        let mut font = FontDescription::new();
        font.set_family(family);
        font.set_absolute_size(size_pt * f64::from(pango::SCALE));
        Self { font, metrics: None }
    }

    /// Measure the font. Cached: this costs a Pango layout and the answer only
    /// changes when the font does.
    pub fn metrics(&mut self, cr: &Context) -> CellMetrics {
        if let Some(m) = self.metrics {
            return m;
        }
        let layout = pangocairo::functions::create_layout(cr);
        layout.set_font_description(Some(&self.font));
        // A run of identical glyphs divides out any per-layout padding.
        layout.set_text("MMMMMMMMMM");
        let (w, h) = layout.pixel_size();
        let m =
            CellMetrics { width: (f64::from(w) / 10.0).max(1.0), height: f64::from(h).max(1.0) };
        self.metrics = Some(m);
        m
    }

    /// Grid size that fits the given pixel area.
    pub fn grid_for(&mut self, cr: &Context, width: f64, height: f64) -> (u16, u16, CellMetrics) {
        let m = self.metrics(cr);
        (grid_axis(width, m.width), grid_axis(height, m.height), m)
    }

    pub fn draw(&mut self, cr: &Context, snapshot: &Snapshot, width: f64, height: f64) {
        let m = self.metrics(cr);

        set_source(cr, snapshot.background);
        let _ = cr.paint();

        let layout = pangocairo::functions::create_layout(cr);
        layout.set_font_description(Some(&self.font));

        // Backgrounds first, as a separate pass: drawing each cell's background
        // immediately before its glyph would let a wide glyph's overhang be
        // clipped by the next cell's fill.
        for row in 0..snapshot.rows {
            for col in 0..snapshot.cols {
                let Some(cell) = snapshot.cell(col, row) else { continue };
                if cell.bg == snapshot.background {
                    continue;
                }
                set_source(cr, cell.bg);
                cr.rectangle(
                    f64::from(col) * m.width,
                    f64::from(row) * m.height,
                    m.width,
                    m.height,
                );
                let _ = cr.fill();
            }
        }

        for row in 0..snapshot.rows {
            for col in 0..snapshot.cols {
                let Some(cell) = snapshot.cell(col, row) else { continue };
                if cell.width == CellWidth::Spacer {
                    continue;
                }
                let text = snapshot.cell_text(cell);
                if text.trim_matches(' ').is_empty() {
                    continue;
                }

                let attrs = pango::AttrList::new();
                if cell.style.bold {
                    attrs.insert(pango::AttrInt::new_weight(pango::Weight::Bold));
                }
                if cell.style.italic {
                    attrs.insert(pango::AttrInt::new_style(pango::Style::Italic));
                }
                if cell.style.underline {
                    attrs.insert(pango::AttrInt::new_underline(pango::Underline::Single));
                }
                if cell.style.strikethrough {
                    attrs.insert(pango::AttrInt::new_strikethrough(true));
                }
                layout.set_attributes(Some(&attrs));
                layout.set_text(text);

                let fg = if cell.style.faint {
                    blend(cell.fg, snapshot.background, 0.5)
                } else {
                    cell.fg
                };
                set_source(cr, fg);
                cr.move_to(f64::from(col) * m.width, f64::from(row) * m.height);
                pangocairo::functions::show_layout(cr, &layout);
            }
        }

        if let Some(cursor) = snapshot.cursor {
            let x = f64::from(cursor.col) * m.width;
            let y = f64::from(cursor.row) * m.height;
            set_source(cr, cursor.color);
            match cursor.shape {
                CursorShape::Block => {
                    cr.rectangle(x, y, m.width, m.height);
                    cr.set_operator(cairo::Operator::Difference);
                    let _ = cr.fill();
                    cr.set_operator(cairo::Operator::Over);
                }
                CursorShape::Bar => {
                    cr.rectangle(x, y, 2.0, m.height);
                    let _ = cr.fill();
                }
                CursorShape::Underline => {
                    cr.rectangle(x, y + m.height - 2.0, m.width, 2.0);
                    let _ = cr.fill();
                }
            }
        }

        // Any area below the last row belongs to the terminal background too.
        let used = f64::from(snapshot.rows) * m.height;
        if used < height {
            set_source(cr, snapshot.background);
            cr.rectangle(0.0, used, width, height - used);
            let _ = cr.fill();
        }
    }
}

fn set_source(cr: &Context, c: Rgb) {
    cr.set_source_rgb(f64::from(c.r) / 255.0, f64::from(c.g) / 255.0, f64::from(c.b) / 255.0);
}

/// Cells that fit along one axis, as a valid grid dimension.
///
/// A grid dimension is a `u16` and zero is not a legal terminal size, so the
/// result is clamped into `1..=u16::MAX` while still in `f64`. Doing it there
/// rather than after the conversion is what gives NaN and infinity a defined
/// answer instead of a saturated one.
fn grid_axis(pixels: f64, cell: f64) -> u16 {
    let fitted = (pixels / cell).floor();
    if !fitted.is_finite() || fitted < 1.0 {
        return 1;
    }
    if fitted >= f64::from(u16::MAX) {
        return u16::MAX;
    }
    // Integral and inside `1..u16::MAX` by the two guards above, so this is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let cells = fitted as u16;
    cells
}

fn blend(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let mix = |x: u8, y: u8| {
        let v = f64::from(x) * (1.0 - t) + f64::from(y) * t;
        // Both endpoints are `u8` and `t` is a ratio, so this already lands in
        // `0.0..=255.0`; the clamp keeps that true for an out-of-range `t` too.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let byte = v.clamp(0.0, 255.0) as u8;
        byte
    };
    Rgb::new(mix(a.r, b.r), mix(a.g, b.g), mix(a.b, b.b))
}
