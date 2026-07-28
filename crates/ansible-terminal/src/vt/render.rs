//! Incremental render state: the bridge from terminal state to a renderer.

use std::ffi::c_void;

use crate::Result;
use crate::snapshot::{CellStyle, CellWidth, Cursor, CursorShape, Rgb, Snapshot, SnapshotBuilder};
use crate::sys;

use super::{Terminal, check, sized};

/// Owns a `GhosttyRenderState` plus the two reusable iterators.
///
/// The iterators are allocated once and refilled per frame, which is what the
/// upstream API is designed for and keeps a redraw allocation-free apart from
/// the snapshot itself.
pub struct RenderState {
    raw: sys::GhosttyRenderState,
    rows: sys::GhosttyRenderStateRowIterator,
    cells: sys::GhosttyRenderStateRowCells,
}

// SAFETY: the handles are owned exclusively by this struct and every method
// takes `&mut self`.
unsafe impl Send for RenderState {}

// `clippy::similar_names` fires on the `default_fg`/`default_bg` parameter pairs
// below. Foreground/background is the domain's own naming and the abbreviations
// are used throughout this crate; renaming them to satisfy the lint would make
// the code harder to read, not easier.
#[allow(clippy::similar_names)]
impl RenderState {
    /// Allocate the render state and the two reusable iterators.
    ///
    /// # Errors
    /// [`crate::Error::Vt`] if libghostty fails to allocate any of the three
    /// handles.
    pub fn new() -> Result<Self> {
        let mut raw: sys::GhosttyRenderState = std::ptr::null_mut();
        // SAFETY: valid out-pointer; null allocator selects the default.
        check("ghostty_render_state_new", unsafe {
            sys::ghostty_render_state_new(std::ptr::null(), &raw mut raw)
        })?;

        let mut rows: sys::GhosttyRenderStateRowIterator = std::ptr::null_mut();
        // SAFETY: valid out-pointer; null allocator selects the default.
        check("ghostty_render_state_row_iterator_new", unsafe {
            sys::ghostty_render_state_row_iterator_new(std::ptr::null(), &raw mut rows)
        })?;

        let mut cells: sys::GhosttyRenderStateRowCells = std::ptr::null_mut();
        // SAFETY: valid out-pointer; null allocator selects the default.
        check("ghostty_render_state_row_cells_new", unsafe {
            sys::ghostty_render_state_row_cells_new(std::ptr::null(), &raw mut cells)
        })?;

        Ok(Self { raw, rows, cells })
    }

    /// Refresh from the terminal and copy the viewport into a [`Snapshot`].
    ///
    /// # Errors
    /// [`crate::Error::Vt`] if libghostty fails to refresh the render state or
    /// to report the viewport dimensions, colors, or cursor.
    pub fn snapshot(&mut self, terminal: &mut Terminal) -> Result<Snapshot> {
        // SAFETY: both handles are valid.
        check("ghostty_render_state_update", unsafe {
            sys::ghostty_render_state_update(self.raw, terminal.raw())
        })?;

        let mut colors: sys::GhosttyRenderStateColors = sized();
        // SAFETY: `colors` was size-stamped by `sized()` and is a valid
        // out-pointer.
        check("ghostty_render_state_colors_get", unsafe {
            sys::ghostty_render_state_colors_get(self.raw, &raw mut colors)
        })?;
        let foreground = rgb(colors.foreground);
        let background = rgb(colors.background);

        let cols: u16 = self.get(sys::GHOSTTY_RENDER_STATE_DATA_COLS)?;
        let rows: u16 = self.get(sys::GHOSTTY_RENDER_STATE_DATA_ROWS)?;

        let mut builder = SnapshotBuilder::new(cols, rows, foreground, background);
        builder.set_cursor(self.cursor(&colors)?);

        // Point the row iterator at the current frame.
        // SAFETY: `self.rows` is a valid out-pointer for the iterator handle
        // that `DATA_ROW_ITERATOR` writes.
        check("ghostty_render_state_get(ROW_ITERATOR)", unsafe {
            sys::ghostty_render_state_get(
                self.raw,
                sys::GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                (&raw mut self.rows).cast::<c_void>(),
            )
        })?;

        let mut utf8 = [0u8; 64];
        // SAFETY: the iterator is positioned by the call above.
        while unsafe { sys::ghostty_render_state_row_iterator_next(self.rows) } {
            // SAFETY: the iterator is positioned on a row by the loop condition,
            // and `self.cells` is a valid out-pointer for the cells handle.
            check("ghostty_render_state_row_get(CELLS)", unsafe {
                sys::ghostty_render_state_row_get(
                    self.rows,
                    sys::GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                    (&raw mut self.cells).cast::<c_void>(),
                )
            })?;

            let mut col = 0u16;
            // SAFETY: the cells handle was just refilled for this row.
            while unsafe { sys::ghostty_render_state_row_cells_next(self.cells) } {
                if col >= cols {
                    break;
                }
                self.push_cell(&mut builder, &mut utf8, foreground, background);
                col += 1;
            }

            // Pad short rows so the grid stays rectangular.
            while col < cols {
                builder.push_cell(
                    "",
                    foreground,
                    background,
                    CellStyle::default(),
                    CellWidth::Narrow,
                );
                col += 1;
            }
        }

        Ok(builder.build())
    }

    fn push_cell(
        &mut self,
        builder: &mut SnapshotBuilder,
        utf8: &mut [u8; 64],
        default_fg: Rgb,
        default_bg: Rgb,
    ) {
        let width = self.cell_width();

        // Spacers carry no text and must not repeat the wide glyph.
        if width == CellWidth::Spacer {
            builder.push_cell("", default_fg, default_bg, CellStyle::default(), width);
            return;
        }

        let text = self.cell_text(utf8);
        let (style, fg, bg) = self.cell_style(default_fg, default_bg);
        builder.push_cell(text, fg, bg, style, width);
    }

    /// Grapheme cluster as UTF-8. Clusters longer than the scratch buffer are
    /// rare enough (long ZWJ emoji sequences) that dropping them is acceptable
    /// for the spike; they render as blank rather than as mojibake.
    fn cell_text<'a>(&mut self, utf8: &'a mut [u8; 64]) -> &'a str {
        let mut buf = sys::GhosttyBuffer { ptr: utf8.as_mut_ptr(), cap: utf8.len(), len: 0 };
        // SAFETY: `buf` describes a valid writable region.
        let result = unsafe {
            sys::ghostty_render_state_row_cells_get(
                self.cells,
                sys::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_UTF8,
                (&raw mut buf).cast::<c_void>(),
            )
        };
        if result != sys::GHOSTTY_SUCCESS || buf.len == 0 || buf.len > utf8.len() {
            return "";
        }
        std::str::from_utf8(&utf8[..buf.len]).unwrap_or("")
    }

    fn cell_width(&mut self) -> CellWidth {
        // SAFETY: `GhosttyCell` is an opaque handle (a raw pointer), for which
        // all-zeroes is null. It is only read after the call below reports
        // success, so a null handle is never passed on.
        let mut cell: sys::GhosttyCell = unsafe { std::mem::zeroed() };
        // SAFETY: valid out-pointer for the raw cell.
        let got = unsafe {
            sys::ghostty_render_state_row_cells_get(
                self.cells,
                sys::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW,
                (&raw mut cell).cast::<c_void>(),
            )
        };
        if got != sys::GHOSTTY_SUCCESS {
            return CellWidth::Narrow;
        }

        let mut wide = sys::GHOSTTY_CELL_WIDE_NARROW;
        // SAFETY: `cell` is a valid cell handle for this iterator position.
        let got = unsafe {
            sys::ghostty_cell_get(
                cell,
                sys::GHOSTTY_CELL_DATA_WIDE,
                (&raw mut wide).cast::<c_void>(),
            )
        };
        if got != sys::GHOSTTY_SUCCESS {
            return CellWidth::Narrow;
        }

        match wide {
            sys::GHOSTTY_CELL_WIDE_WIDE => CellWidth::Wide,
            sys::GHOSTTY_CELL_WIDE_SPACER_TAIL | sys::GHOSTTY_CELL_WIDE_SPACER_HEAD => {
                CellWidth::Spacer
            }
            _ => CellWidth::Narrow,
        }
    }

    /// Style plus resolved colors.
    ///
    /// `FG_COLOR`/`BG_COLOR` already flatten palette lookups and content tags;
    /// they return `GHOSTTY_INVALID_VALUE` when the cell has no explicit color,
    /// which is the signal to fall back to the terminal default.
    fn cell_style(&mut self, default_fg: Rgb, default_bg: Rgb) -> (CellStyle, Rgb, Rgb) {
        let mut has_styling = false;
        // SAFETY: valid out-pointer for a bool.
        unsafe {
            sys::ghostty_render_state_row_cells_get(
                self.cells,
                sys::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_HAS_STYLING,
                (&raw mut has_styling).cast::<c_void>(),
            );
        }

        let mut style = CellStyle::default();
        if has_styling {
            let mut raw: sys::GhosttyStyle = sized();
            // SAFETY: sized struct with a valid out-pointer.
            let got = unsafe {
                sys::ghostty_render_state_row_cells_get(
                    self.cells,
                    sys::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
                    (&raw mut raw).cast::<c_void>(),
                )
            };
            if got == sys::GHOSTTY_SUCCESS {
                // `GhosttyStyle::underline` is declared `int` while the
                // `GHOSTTY_SGR_UNDERLINE_*` constants are `unsigned int`, so
                // there is no cast between the two that is correct for every
                // value. Since "none" is zero, test for it directly and pin
                // that assumption to a compile-time check.
                const _: () = assert!(sys::GHOSTTY_SGR_UNDERLINE_NONE.0 == 0);
                style = CellStyle {
                    bold: raw.bold,
                    italic: raw.italic,
                    underline: raw.underline != 0,
                    strikethrough: raw.strikethrough,
                    inverse: raw.inverse,
                    faint: raw.faint,
                };
            }
        }

        let fg = self
            .cell_color(sys::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR)
            .unwrap_or(default_fg);
        let bg = self
            .cell_color(sys::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR)
            .unwrap_or(default_bg);

        // Applying inverse here keeps every renderer from re-implementing it.
        if style.inverse { (style, bg, fg) } else { (style, fg, bg) }
    }

    fn cell_color(&mut self, kind: sys::GhosttyRenderStateRowCellsData) -> Option<Rgb> {
        let mut color = sys::GhosttyColorRgb::default();
        // SAFETY: valid out-pointer for a GhosttyColorRgb.
        let got = unsafe {
            sys::ghostty_render_state_row_cells_get(
                self.cells,
                kind,
                (&raw mut color).cast::<c_void>(),
            )
        };
        (got == sys::GHOSTTY_SUCCESS).then(|| rgb(color))
    }

    fn cursor(&mut self, colors: &sys::GhosttyRenderStateColors) -> Result<Option<Cursor>> {
        let visible: bool = self.get(sys::GHOSTTY_RENDER_STATE_DATA_CURSOR_VISIBLE)?;
        let in_viewport: bool =
            self.get(sys::GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_HAS_VALUE)?;
        if !visible || !in_viewport {
            return Ok(None);
        }

        let col: u16 = self.get(sys::GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_X)?;
        let row: u16 = self.get(sys::GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_Y)?;
        let mut style = sys::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK;
        // SAFETY: `style` is a valid out-pointer for the enum that
        // `DATA_CURSOR_VISUAL_STYLE` writes.
        check("ghostty_render_state_get(CURSOR_VISUAL_STYLE)", unsafe {
            sys::ghostty_render_state_get(
                self.raw,
                sys::GHOSTTY_RENDER_STATE_DATA_CURSOR_VISUAL_STYLE,
                (&raw mut style).cast::<c_void>(),
            )
        })?;

        let shape = match style {
            sys::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR => CursorShape::Bar,
            sys::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE => CursorShape::Underline,
            _ => CursorShape::Block,
        };

        let color =
            if colors.cursor_has_value { rgb(colors.cursor) } else { rgb(colors.foreground) };
        Ok(Some(Cursor { col, row, shape, color }))
    }

    /// Read a scalar out of the render state.
    ///
    /// # Safety contract
    /// `T` must match the output type documented for `kind` in render.h.
    fn get<T: Default>(&mut self, kind: sys::GhosttyRenderStateData) -> Result<T> {
        let mut out = T::default();
        // SAFETY: `out` is a valid out-pointer, and every caller in this module
        // instantiates `T` as the type render.h documents for `kind` — the
        // contract stated above.
        check("ghostty_render_state_get", unsafe {
            sys::ghostty_render_state_get(self.raw, kind, (&raw mut out).cast::<c_void>())
        })?;
        Ok(out)
    }
}

impl Drop for RenderState {
    fn drop(&mut self) {
        // SAFETY: each handle was created by its matching `_new` and is freed once.
        unsafe {
            sys::ghostty_render_state_row_cells_free(self.cells);
            sys::ghostty_render_state_row_iterator_free(self.rows);
            sys::ghostty_render_state_free(self.raw);
        }
    }
}

fn rgb(c: sys::GhosttyColorRgb) -> Rgb {
    Rgb::new(c.r, c.g, c.b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TerminalSize;
    use crate::vt::TerminalCallbacks;
    use std::sync::mpsc::channel;

    fn terminal(cols: u16, rows: u16) -> Terminal {
        let (write_pty, write_rx) = channel();
        let (title, title_rx) = channel();
        let (bell, bell_rx) = channel();
        // Leak the receivers: these tests only care about screen contents, and
        // a dropped receiver would make the senders fail silently anyway.
        std::mem::forget((write_rx, title_rx, bell_rx));
        Terminal::new(
            TerminalSize::new(cols, rows, 8, 16),
            1000,
            TerminalCallbacks { write_pty, title, bell },
        )
        .expect("terminal")
    }

    fn render(term: &mut Terminal) -> Snapshot {
        RenderState::new().expect("render state").snapshot(term).expect("snapshot")
    }

    #[test]
    fn plain_text_lands_in_the_grid() {
        let mut term = terminal(20, 3);
        term.write_vt(b"hello");
        let snap = render(&mut term);
        assert_eq!(snap.cols, 20);
        assert_eq!(snap.rows, 3);
        assert_eq!(snap.row_text(0), "hello");
    }

    #[test]
    fn newlines_advance_rows() {
        let mut term = terminal(20, 4);
        term.write_vt(b"one\r\ntwo\r\nthree");
        let snap = render(&mut term);
        assert_eq!(snap.row_text(0), "one");
        assert_eq!(snap.row_text(1), "two");
        assert_eq!(snap.row_text(2), "three");
    }

    #[test]
    fn truecolor_sgr_reaches_the_cell() {
        let mut term = terminal(10, 1);
        term.write_vt(b"\x1b[38;2;255;128;0mX\x1b[0m");
        let snap = render(&mut term);
        let cell = snap.cell(0, 0).unwrap();
        assert_eq!(snap.cell_text(cell), "X");
        assert_eq!(cell.fg, Rgb::new(255, 128, 0));
    }

    #[test]
    fn palette_colors_resolve_through_the_palette() {
        let mut term = terminal(10, 1);
        // SGR 31 is palette index 1 (red); the exact RGB comes from the
        // default palette, so assert it resolved to something non-default.
        term.write_vt(b"\x1b[31mR\x1b[0m");
        let snap = render(&mut term);
        let cell = snap.cell(0, 0).unwrap();
        assert_ne!(cell.fg, snap.foreground);
    }

    #[test]
    fn text_attributes_are_reported() {
        let mut term = terminal(10, 1);
        term.write_vt(b"\x1b[1;3;4mA\x1b[0m");
        let snap = render(&mut term);
        let style = snap.cell(0, 0).unwrap().style;
        assert!(style.bold, "bold");
        assert!(style.italic, "italic");
        assert!(style.underline, "underline");
    }

    #[test]
    fn inverse_swaps_foreground_and_background() {
        let mut term = terminal(10, 1);
        term.write_vt(b"\x1b[38;2;10;20;30m\x1b[48;2;200;100;50m\x1b[7mA\x1b[0m");
        let snap = render(&mut term);
        let cell = snap.cell(0, 0).unwrap();
        assert_eq!(cell.fg, Rgb::new(200, 100, 50));
        assert_eq!(cell.bg, Rgb::new(10, 20, 30));
    }

    #[test]
    fn box_drawing_characters_survive_as_utf8() {
        let mut term = terminal(10, 1);
        term.write_vt("┌─┬─┐".as_bytes());
        let snap = render(&mut term);
        assert_eq!(snap.row_text(0), "┌─┬─┐");
    }

    #[test]
    fn wide_glyphs_occupy_two_cells_without_duplicating() {
        let mut term = terminal(10, 1);
        term.write_vt("漢字".as_bytes());
        let snap = render(&mut term);
        assert_eq!(snap.row_text(0), "漢字");
        assert_eq!(snap.cell(0, 0).unwrap().width, CellWidth::Wide);
        assert_eq!(snap.cell(1, 0).unwrap().width, CellWidth::Spacer);
    }

    #[test]
    fn cursor_position_tracks_output() {
        let mut term = terminal(20, 3);
        term.write_vt(b"abc");
        let snap = render(&mut term);
        let cursor = snap.cursor.expect("cursor visible");
        assert_eq!((cursor.col, cursor.row), (3, 0));
    }

    #[test]
    fn hiding_the_cursor_clears_it() {
        let mut term = terminal(20, 3);
        term.write_vt(b"\x1b[?25l");
        assert!(render(&mut term).cursor.is_none());
    }

    #[test]
    fn resize_reflows_and_reports_new_dimensions() {
        let mut term = terminal(10, 4);
        term.write_vt(b"0123456789abcde");
        term.resize(TerminalSize::new(20, 4, 8, 16)).unwrap();
        let snap = render(&mut term);
        assert_eq!(snap.cols, 20);
        assert_eq!(snap.row_text(0), "0123456789abcde");
    }

    #[test]
    fn erase_display_clears_the_screen() {
        let mut term = terminal(10, 2);
        term.write_vt(b"junk\r\nmore");
        term.write_vt(b"\x1b[2J\x1b[H");
        assert_eq!(render(&mut term).screen_text(), "");
    }

    #[test]
    fn scrolling_past_the_bottom_keeps_the_last_rows() {
        let mut term = terminal(10, 2);
        term.write_vt(b"a\r\nb\r\nc");
        let snap = render(&mut term);
        assert_eq!(snap.row_text(0), "b");
        assert_eq!(snap.row_text(1), "c");
    }

    #[test]
    fn carriage_return_overwrites_in_place() {
        let mut term = terminal(10, 1);
        term.write_vt(b"xxxxx\ryo");
        assert_eq!(render(&mut term).row_text(0), "yoxxx");
    }
}
