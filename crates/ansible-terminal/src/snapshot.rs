//! A renderable copy of the visible screen.
//!
//! Renderers consume this instead of the byte stream. Grapheme text for every
//! cell lives in one shared `String` so a frame costs a single allocation
//! rather than one per cell, which matters under sustained high-volume output.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const BLACK: Self = Self { r: 0, g: 0, b: 0 };
    pub const WHITE: Self = Self { r: 0xff, g: 0xff, b: 0xff };

    #[must_use]
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// The SGR text attributes a cell can carry.
//
// `clippy::struct_excessive_bools` wants a bitflags type here. These six are not
// a state machine to be encoded — they are exactly the independent SGR
// attributes libghostty reports, and renderers read them by name. A bitfield
// would trade that legibility for nothing.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellStyle {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub inverse: bool,
    pub faint: bool,
}

/// How a cell relates to a multi-column grapheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CellWidth {
    #[default]
    Narrow,
    /// Leading cell of a double-width grapheme.
    Wide,
    /// Trailing filler owned by the `Wide` cell to its left; draws nothing.
    Spacer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    text_start: u32,
    text_len: u32,
    pub fg: Rgb,
    pub bg: Rgb,
    pub style: CellStyle,
    pub width: CellWidth,
}

impl Cell {
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.text_len == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Bar,
    Underline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub col: u16,
    pub row: u16,
    pub shape: CursorShape,
    pub color: Rgb,
}

/// One frame of terminal state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub cols: u16,
    pub rows: u16,
    pub background: Rgb,
    pub foreground: Rgb,
    /// `None` when the cursor is hidden or scrolled out of the viewport.
    pub cursor: Option<Cursor>,
    text: String,
    cells: Vec<Cell>,
}

impl Snapshot {
    #[must_use]
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// Cell at `(col, row)`, or `None` if out of bounds.
    #[must_use]
    pub fn cell(&self, col: u16, row: u16) -> Option<&Cell> {
        if col >= self.cols || row >= self.rows {
            return None;
        }
        self.cells.get(usize::from(row) * usize::from(self.cols) + usize::from(col))
    }

    /// Grapheme text for a cell. Empty for blank cells and wide-cell spacers.
    #[must_use]
    pub fn cell_text(&self, cell: &Cell) -> &str {
        let start = cell.text_start as usize;
        let end = start + cell.text_len as usize;
        self.text.get(start..end).unwrap_or("")
    }

    /// Visible text of one row with trailing blanks trimmed.
    ///
    /// This is the assertion surface for tests and the deterministic fixture:
    /// it is how we check that libghostty parsed a sequence the way we expect.
    #[must_use]
    pub fn row_text(&self, row: u16) -> String {
        let mut out = String::new();
        for col in 0..self.cols {
            match self.cell(col, row) {
                Some(cell) if cell.width == CellWidth::Spacer => {}
                Some(cell) if cell.is_blank() => out.push(' '),
                Some(cell) => out.push_str(self.cell_text(cell)),
                None => break,
            }
        }
        while out.ends_with(' ') {
            out.pop();
        }
        out
    }

    /// Whole screen as text, trailing blank lines trimmed.
    #[must_use]
    pub fn screen_text(&self) -> String {
        let mut lines: Vec<String> = (0..self.rows).map(|r| self.row_text(r)).collect();
        while lines.last().is_some_and(std::string::String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }
}

/// Accumulates cells while walking libghostty's render state.
pub struct SnapshotBuilder {
    cols: u16,
    rows: u16,
    background: Rgb,
    foreground: Rgb,
    cursor: Option<Cursor>,
    text: String,
    cells: Vec<Cell>,
}

impl SnapshotBuilder {
    #[must_use]
    pub fn new(cols: u16, rows: u16, foreground: Rgb, background: Rgb) -> Self {
        let capacity = usize::from(cols) * usize::from(rows);
        Self {
            cols,
            rows,
            background,
            foreground,
            cursor: None,
            text: String::with_capacity(capacity),
            cells: Vec::with_capacity(capacity),
        }
    }

    pub fn set_cursor(&mut self, cursor: Option<Cursor>) {
        self.cursor = cursor;
    }

    /// Offsets for text appended at the current end of the buffer.
    ///
    /// `None` when the range would not fit the `u32` offsets a [`Cell`] stores.
    /// Reaching that needs a grid far larger than memory allows, but a truncated
    /// offset would slice the wrong bytes instead of failing, so the caller
    /// stores a blank cell rather than a wrong one.
    fn offsets_for(&self, text: &str) -> Option<(u32, u32)> {
        let start = u32::try_from(self.text.len()).ok()?;
        let len = u32::try_from(text.len()).ok()?;
        start.checked_add(len)?;
        Some((start, len))
    }

    pub fn push_cell(&mut self, text: &str, fg: Rgb, bg: Rgb, style: CellStyle, width: CellWidth) {
        let (text_start, text_len) = match self.offsets_for(text) {
            Some(offsets) => {
                self.text.push_str(text);
                offsets
            }
            None => (0, 0),
        };
        self.cells.push(Cell { text_start, text_len, fg, bg, style, width });
    }

    #[must_use]
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    /// Pad to a full `cols * rows` grid so renderers can index without bounds
    /// juggling when libghostty yields a short final row.
    #[must_use]
    pub fn build(mut self) -> Snapshot {
        let expected = usize::from(self.cols) * usize::from(self.rows);
        while self.cells.len() < expected {
            self.push_cell(
                "",
                self.foreground,
                self.background,
                CellStyle::default(),
                CellWidth::Narrow,
            );
        }
        self.cells.truncate(expected);
        Snapshot {
            cols: self.cols,
            rows: self.rows,
            background: self.background,
            foreground: self.foreground,
            cursor: self.cursor,
            text: self.text,
            cells: self.cells,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_row(texts: &[&str]) -> Snapshot {
        let cols = u16::try_from(texts.len()).expect("fixture row fits a u16 grid");
        let mut b = SnapshotBuilder::new(cols, 1, Rgb::WHITE, Rgb::BLACK);
        for t in texts {
            b.push_cell(t, Rgb::WHITE, Rgb::BLACK, CellStyle::default(), CellWidth::Narrow);
        }
        b.build()
    }

    #[test]
    fn row_text_joins_cells_and_trims_trailing_blanks() {
        let snap = build_row(&["h", "i", "", ""]);
        assert_eq!(snap.row_text(0), "hi");
    }

    #[test]
    fn blank_cells_between_text_become_spaces() {
        let snap = build_row(&["a", "", "b"]);
        assert_eq!(snap.row_text(0), "a b");
    }

    #[test]
    fn cell_text_is_isolated_per_cell() {
        let snap = build_row(&["ab", "c"]);
        assert_eq!(snap.cell_text(snap.cell(0, 0).unwrap()), "ab");
        assert_eq!(snap.cell_text(snap.cell(1, 0).unwrap()), "c");
    }

    #[test]
    fn builder_pads_short_grids() {
        let mut b = SnapshotBuilder::new(4, 2, Rgb::WHITE, Rgb::BLACK);
        b.push_cell("x", Rgb::WHITE, Rgb::BLACK, CellStyle::default(), CellWidth::Narrow);
        let snap = b.build();
        assert_eq!(snap.cells().len(), 8);
        assert_eq!(snap.row_text(0), "x");
        assert_eq!(snap.row_text(1), "");
    }

    #[test]
    fn spacers_are_skipped_so_wide_glyphs_are_not_duplicated() {
        let mut b = SnapshotBuilder::new(3, 1, Rgb::WHITE, Rgb::BLACK);
        b.push_cell("漢", Rgb::WHITE, Rgb::BLACK, CellStyle::default(), CellWidth::Wide);
        b.push_cell("", Rgb::WHITE, Rgb::BLACK, CellStyle::default(), CellWidth::Spacer);
        b.push_cell("!", Rgb::WHITE, Rgb::BLACK, CellStyle::default(), CellWidth::Narrow);
        let snap = b.build();
        assert_eq!(snap.row_text(0), "漢!");
    }

    #[test]
    fn out_of_bounds_cell_lookup_is_none() {
        let snap = build_row(&["a"]);
        assert!(snap.cell(1, 0).is_none());
        assert!(snap.cell(0, 1).is_none());
    }

    #[test]
    fn screen_text_drops_trailing_blank_rows() {
        let mut b = SnapshotBuilder::new(2, 3, Rgb::WHITE, Rgb::BLACK);
        for t in ["a", "b"] {
            b.push_cell(t, Rgb::WHITE, Rgb::BLACK, CellStyle::default(), CellWidth::Narrow);
        }
        let snap = b.build();
        assert_eq!(snap.screen_text(), "ab");
    }
}
