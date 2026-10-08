#![cfg_attr(not(test), allow(dead_code))]

use std::borrow::Borrow;
use std::ops::Range;

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::{Cell, CellWidth};
use ratatui::layout::{Position, Size};
use ratatui::style::Color;

use crate::terminal_capabilities::TerminalColorLevel;

/// The character Kitty's unicode-placeholder image protocol puts in every cell
/// an image covers. Such a cell's foreground color is the image id rather than
/// a color to show, so it has to reach the terminal unchanged.
const KITTY_IMAGE_PLACEHOLDER: char = '\u{10EEEE}';

/// Sets the foreground back to the terminal's default.
const DEFAULT_FOREGROUND: &str = "\x1b[39m";

pub(crate) struct CapabilityBackend<B> {
    inner: B,
    color_level: TerminalColorLevel,
}

impl<B> CapabilityBackend<B> {
    pub(crate) const fn new(inner: B, color_level: TerminalColorLevel) -> Self {
        Self { inner, color_level }
    }

    pub(crate) const fn inner(&self) -> &B {
        &self.inner
    }

    pub(crate) fn inner_mut(&mut self) -> &mut B {
        &mut self.inner
    }

    /// Gives a Kitty placeholder cell back the image id its colors were
    /// adapted away from. NO_COLOR, which is what selects Monochrome, also
    /// stops crossterm from writing any color, so there the id goes into the
    /// symbol as an escape sequence of its own and the cell keeps no color.
    fn keep_image_id(&self, cell: &mut Cell, image_id: Color) {
        let foreground = match (self.color_level, image_id) {
            (TerminalColorLevel::Monochrome, Color::Rgb(red, green, blue)) => {
                format!("\x1b[38;2;{red};{green};{blue}m")
            }
            (TerminalColorLevel::Monochrome, Color::Indexed(index)) => {
                format!("\x1b[38;5;{index}m")
            }
            _ => {
                cell.fg = image_id;
                return;
            }
        };
        let symbol = format!("{foreground}{}{DEFAULT_FOREGROUND}", cell.symbol());
        cell.set_symbol(&symbol);
    }
}

impl<B: Backend> Backend for CapabilityBackend<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        if self.color_level == TerminalColorLevel::TrueColor {
            let mut updates = content.collect::<Vec<_>>();
            draw_covered_columns_first(&mut updates);
            return self.inner.draw(updates.into_iter());
        }

        let mut adapted = content
            .map(|(x, y, cell)| {
                let mut cell = cell.clone();
                let image_id = cell
                    .symbol()
                    .contains(KITTY_IMAGE_PLACEHOLDER)
                    .then_some(cell.fg);
                cell.set_style(self.color_level.adapt_style(cell.style()));
                if let Some(image_id) = image_id {
                    self.keep_image_id(&mut cell, image_id);
                }
                (x, y, cell)
            })
            .collect::<Vec<_>>();
        draw_covered_columns_first(&mut adapted);
        self.inner
            .draw(adapted.iter().map(|(x, y, cell)| (*x, *y, cell)))
    }

    fn append_lines(&mut self, line_count: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(line_count)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }

    #[allow(deprecated)]
    fn get_cursor(&mut self) -> Result<(u16, u16), Self::Error> {
        self.inner.get_cursor()
    }

    #[allow(deprecated)]
    fn set_cursor(&mut self, x: u16, y: u16) -> Result<(), Self::Error> {
        self.inner.set_cursor(x, y)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }

    fn scroll_region_up(&mut self, region: Range<u16>, line_count: u16) -> Result<(), Self::Error> {
        self.inner.scroll_region_up(region, line_count)
    }

    fn scroll_region_down(
        &mut self,
        region: Range<u16>,
        line_count: u16,
    ) -> Result<(), Self::Error> {
        self.inner.scroll_region_down(region, line_count)
    }
}

/// Moves the writes to the columns a wide cell covers ahead of that cell.
///
/// ratatui-core 0.1.2 follows an emoji carrying U+FE0F, such as ⚠️, with a
/// write to the column the emoji covers, and the crossterm backend prints that
/// write without moving the cursor, as if the emoji had advanced it by one
/// column. Terminals that draw the emoji in two columns, Ghostty among them,
/// advance it by two, so that write and the rest of the row land one column
/// to the right. Written first, the covered column is cleared on terminals
/// that draw the emoji in one column and drawn over on the others, and the
/// emoji gets its own cursor move. ratatui fixes its diff the same way in
/// ratatui/ratatui#2686, which no release has yet.
fn draw_covered_columns_first<C: Borrow<Cell>>(updates: &mut [(u16, u16, C)]) {
    let mut index = 0;
    while index < updates.len() {
        let (x, y, cell) = &updates[index];
        let (x, y) = (*x, *y);
        let covered = updates[index + 1..]
            .iter()
            .zip(1..cell.borrow().cell_width())
            .take_while(|((next_x, next_y, _), offset)| {
                *next_y == y && next_x.checked_sub(x) == Some(*offset)
            })
            .count();
        updates[index..=index + covered].rotate_left(1);
        index += covered + 1;
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io;
    use std::num::NonZeroU16;
    use std::ops::Range;

    use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
    use ratatui::buffer::{Buffer, Cell, CellDiffOption};
    use ratatui::layout::{Position, Rect, Size};
    use ratatui::style::{Color, Modifier, Style};
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;

    use super::CapabilityBackend;
    use crate::terminal_capabilities::TerminalColorLevel;

    #[derive(Debug, Eq, PartialEq)]
    enum BackendCall {
        AppendLines(u16),
        HideCursor,
        ShowCursor,
        GetCursorPosition,
        SetCursorPosition(Position),
        GetCursor,
        SetCursor(u16, u16),
        Clear,
        ClearRegion(ClearType),
        Size,
        WindowSize,
        Flush,
        ScrollRegionUp(Range<u16>, u16),
        ScrollRegionDown(Range<u16>, u16),
    }

    struct RecordingBackend {
        drawn: Vec<(u16, u16, Cell)>,
        calls: RefCell<Vec<BackendCall>>,
        cursor_position: Position,
        cursor: (u16, u16),
        size: Size,
        window_size: WindowSize,
    }

    impl Default for RecordingBackend {
        fn default() -> Self {
            Self {
                drawn: Vec::new(),
                calls: RefCell::new(Vec::new()),
                cursor_position: Position { x: 5, y: 7 },
                cursor: (23, 29),
                size: Size::new(80, 24),
                window_size: WindowSize {
                    columns_rows: Size::new(80, 24),
                    pixels: Size::new(800, 480),
                },
            }
        }
    }

    impl Backend for RecordingBackend {
        type Error = io::Error;

        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.drawn
                .extend(content.map(|(x, y, cell)| (x, y, cell.clone())));
            Ok(())
        }

        fn append_lines(&mut self, line_count: u16) -> io::Result<()> {
            self.calls
                .borrow_mut()
                .push(BackendCall::AppendLines(line_count));
            Ok(())
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            self.calls.borrow_mut().push(BackendCall::HideCursor);
            Ok(())
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            self.calls.borrow_mut().push(BackendCall::ShowCursor);
            Ok(())
        }

        fn get_cursor_position(&mut self) -> io::Result<Position> {
            self.calls.borrow_mut().push(BackendCall::GetCursorPosition);
            Ok(self.cursor_position)
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
            let position = position.into();
            self.calls
                .borrow_mut()
                .push(BackendCall::SetCursorPosition(position));
            Ok(())
        }

        #[allow(deprecated)]
        fn get_cursor(&mut self) -> io::Result<(u16, u16)> {
            self.calls.borrow_mut().push(BackendCall::GetCursor);
            Ok(self.cursor)
        }

        #[allow(deprecated)]
        fn set_cursor(&mut self, x: u16, y: u16) -> io::Result<()> {
            self.calls.borrow_mut().push(BackendCall::SetCursor(x, y));
            Ok(())
        }

        fn clear(&mut self) -> io::Result<()> {
            self.calls.borrow_mut().push(BackendCall::Clear);
            Ok(())
        }

        fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
            self.calls
                .borrow_mut()
                .push(BackendCall::ClearRegion(clear_type));
            Ok(())
        }

        fn size(&self) -> io::Result<Size> {
            self.calls.borrow_mut().push(BackendCall::Size);
            Ok(self.size)
        }

        fn window_size(&mut self) -> io::Result<WindowSize> {
            self.calls.borrow_mut().push(BackendCall::WindowSize);
            Ok(self.window_size)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.calls.borrow_mut().push(BackendCall::Flush);
            Ok(())
        }

        fn scroll_region_up(&mut self, region: Range<u16>, line_count: u16) -> io::Result<()> {
            self.calls
                .borrow_mut()
                .push(BackendCall::ScrollRegionUp(region, line_count));
            Ok(())
        }

        fn scroll_region_down(&mut self, region: Range<u16>, line_count: u16) -> io::Result<()> {
            self.calls
                .borrow_mut()
                .push(BackendCall::ScrollRegionDown(region, line_count));
            Ok(())
        }
    }

    #[derive(Default)]
    struct FailingBackend;

    impl FailingBackend {
        fn error() -> io::Error {
            io::Error::new(io::ErrorKind::PermissionDenied, "injected backend failure")
        }
    }

    impl Backend for FailingBackend {
        type Error = io::Error;

        fn draw<'a, I>(&mut self, _content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            Err(Self::error())
        }

        fn append_lines(&mut self, _line_count: u16) -> io::Result<()> {
            Err(Self::error())
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            Err(Self::error())
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            Err(Self::error())
        }

        fn get_cursor_position(&mut self) -> io::Result<Position> {
            Err(Self::error())
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, _position: P) -> io::Result<()> {
            Err(Self::error())
        }

        #[allow(deprecated)]
        fn get_cursor(&mut self) -> io::Result<(u16, u16)> {
            Err(Self::error())
        }

        #[allow(deprecated)]
        fn set_cursor(&mut self, _x: u16, _y: u16) -> io::Result<()> {
            Err(Self::error())
        }

        fn clear(&mut self) -> io::Result<()> {
            Err(Self::error())
        }

        fn clear_region(&mut self, _clear_type: ClearType) -> io::Result<()> {
            Err(Self::error())
        }

        fn size(&self) -> io::Result<Size> {
            Err(Self::error())
        }

        fn window_size(&mut self) -> io::Result<WindowSize> {
            Err(Self::error())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(Self::error())
        }

        fn scroll_region_up(&mut self, _region: Range<u16>, _line_count: u16) -> io::Result<()> {
            Err(Self::error())
        }

        fn scroll_region_down(&mut self, _region: Range<u16>, _line_count: u16) -> io::Result<()> {
            Err(Self::error())
        }
    }

    fn assert_injected_error<T>(result: io::Result<T>) {
        let error = result.err().expect("injected backend error");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "injected backend failure");
    }

    fn cell_colors_fit(level: TerminalColorLevel, cell: &Cell) -> bool {
        let color_fits = |color| match level {
            TerminalColorLevel::TrueColor => true,
            TerminalColorLevel::Ansi256 => !matches!(color, Color::Rgb(..)),
            TerminalColorLevel::Ansi16 => !matches!(color, Color::Rgb(..) | Color::Indexed(_)),
            TerminalColorLevel::Monochrome => color == Color::Reset,
        };

        color_fits(cell.fg) && color_fits(cell.bg) && color_fits(cell.underline_color)
    }

    #[test]
    fn capability_backend_adapts_changed_cells_and_preserves_metadata() {
        let mut source = Cell::default();
        source.set_symbol("界");
        source.set_style(
            Style::default()
                .fg(Color::Rgb(255, 0, 0))
                .bg(Color::Indexed(42))
                .underline_color(Color::Rgb(0, 255, 0))
                .add_modifier(Modifier::BOLD),
        );
        source.set_diff_option(CellDiffOption::Skip);

        for level in [
            TerminalColorLevel::Ansi256,
            TerminalColorLevel::Ansi16,
            TerminalColorLevel::Monochrome,
        ] {
            let recorder = RecordingBackend::default();
            let mut backend = CapabilityBackend::new(recorder, level);
            backend.draw(std::iter::once((3, 4, &source))).unwrap();

            let drawn = &backend.inner().drawn[0];
            assert_eq!((drawn.0, drawn.1), (3, 4));
            assert_eq!(drawn.2.symbol(), "界");
            assert_eq!(drawn.2.modifier, Modifier::BOLD);
            assert_eq!(drawn.2.diff_option, CellDiffOption::Skip);
            assert!(cell_colors_fit(level, &drawn.2));
        }
    }

    /// NO_COLOR, which is what selects Monochrome, also stops crossterm from
    /// writing any color, so a Kitty placeholder's image id travels in the
    /// symbol: set before the placeholder and back to the default after it.
    #[test]
    fn capability_backend_writes_kitty_image_ids_into_the_symbol_in_monochrome() {
        let mut source = Cell::default();
        source.set_symbol("\u{10EEEE}\u{0305}\u{0305}");
        source.set_fg(Color::Rgb(1, 72, 32));
        source.set_diff_option(CellDiffOption::ForcedWidth(NonZeroU16::MIN));

        let mut backend =
            CapabilityBackend::new(RecordingBackend::default(), TerminalColorLevel::Monochrome);
        backend.draw(std::iter::once((3, 4, &source))).unwrap();

        let (x, y, drawn) = &backend.inner().drawn[0];
        assert_eq!((*x, *y), (3, 4));
        assert_eq!(
            drawn.symbol(),
            "\x1b[38;2;1;72;32m\u{10EEEE}\u{0305}\u{0305}\x1b[39m"
        );
        assert_eq!(drawn.fg, Color::Reset);
        assert_eq!(drawn.diff_option, source.diff_option);
    }

    #[test]
    fn capability_backend_adapts_true_color_without_changing_the_cell() {
        let mut source = Cell::default();
        source.set_symbol("界");
        source.set_style(
            Style::default()
                .fg(Color::Rgb(1, 2, 3))
                .bg(Color::Indexed(42))
                .underline_color(Color::Rgb(4, 5, 6))
                .add_modifier(Modifier::BOLD | Modifier::ITALIC),
        );
        source.set_diff_option(CellDiffOption::Skip);

        let mut backend =
            CapabilityBackend::new(RecordingBackend::default(), TerminalColorLevel::TrueColor);
        backend.draw(std::iter::once((3, 4, &source))).unwrap();

        assert_eq!(backend.inner().drawn, vec![(3, 4, source)]);
    }

    #[test]
    #[allow(deprecated)]
    fn capability_backend_delegates_deprecated_cursor_aliases_exactly() {
        let mut backend =
            CapabilityBackend::new(RecordingBackend::default(), TerminalColorLevel::TrueColor);

        assert_eq!(backend.get_cursor().unwrap(), (23, 29));
        backend.set_cursor(31, 37).unwrap();

        assert_eq!(
            *backend.inner().calls.borrow(),
            vec![BackendCall::GetCursor, BackendCall::SetCursor(31, 37)]
        );
    }

    #[test]
    fn capability_backend_delegates_complete_backend_contract() {
        let mut backend =
            CapabilityBackend::new(RecordingBackend::default(), TerminalColorLevel::Ansi16);
        backend.inner_mut().cursor_position = Position { x: 11, y: 13 };

        backend.append_lines(2).unwrap();
        backend.hide_cursor().unwrap();
        backend.show_cursor().unwrap();
        assert_eq!(
            backend.get_cursor_position().unwrap(),
            Position { x: 11, y: 13 }
        );
        backend
            .set_cursor_position(Position { x: 17, y: 19 })
            .unwrap();
        backend.clear().unwrap();
        backend.clear_region(ClearType::CurrentLine).unwrap();
        assert_eq!(backend.size().unwrap(), Size::new(80, 24));
        assert_eq!(
            backend.window_size().unwrap(),
            WindowSize {
                columns_rows: Size::new(80, 24),
                pixels: Size::new(800, 480),
            }
        );
        backend.flush().unwrap();
        backend.scroll_region_up(3..9, 2).unwrap();
        backend.scroll_region_down(4..12, 3).unwrap();

        assert_eq!(
            *backend.inner().calls.borrow(),
            vec![
                BackendCall::AppendLines(2),
                BackendCall::HideCursor,
                BackendCall::ShowCursor,
                BackendCall::GetCursorPosition,
                BackendCall::SetCursorPosition(Position { x: 17, y: 19 }),
                BackendCall::Clear,
                BackendCall::ClearRegion(ClearType::CurrentLine),
                BackendCall::Size,
                BackendCall::WindowSize,
                BackendCall::Flush,
                BackendCall::ScrollRegionUp(3..9, 2),
                BackendCall::ScrollRegionDown(4..12, 3),
            ]
        );
    }

    #[test]
    fn capability_backend_preserves_draw_errors_for_direct_and_degraded_paths() {
        let source = Cell::default();

        for level in [TerminalColorLevel::TrueColor, TerminalColorLevel::Ansi16] {
            let mut backend = CapabilityBackend::new(FailingBackend, level);
            assert_injected_error(backend.draw(std::iter::once((3, 4, &source))));
        }
    }

    #[test]
    #[allow(deprecated)]
    fn capability_backend_preserves_delegated_backend_errors() {
        let mut backend = CapabilityBackend::new(FailingBackend, TerminalColorLevel::TrueColor);

        assert_injected_error(backend.append_lines(2));
        assert_injected_error(backend.hide_cursor());
        assert_injected_error(backend.show_cursor());
        assert_injected_error(backend.get_cursor_position());
        assert_injected_error(backend.set_cursor_position(Position { x: 3, y: 5 }));
        assert_injected_error(backend.get_cursor());
        assert_injected_error(backend.set_cursor(7, 11));
        assert_injected_error(backend.clear());
        assert_injected_error(backend.clear_region(ClearType::AfterCursor));
        assert_injected_error(backend.size());
        assert_injected_error(backend.window_size());
        assert_injected_error(backend.flush());
        assert_injected_error(backend.scroll_region_up(3..9, 2));
        assert_injected_error(backend.scroll_region_down(4..12, 3));
    }

    /// What a terminal shows, row by row. `None` is a column covered by the
    /// wide grapheme to its left.
    type Screen = Vec<Vec<Option<String>>>;

    /// How many columns a terminal gives `grapheme`. Terminals disagree on one
    /// carrying U+FE0F: most, Ghostty among them, give it two, some one.
    fn columns(grapheme: &str, vs16_columns: usize) -> usize {
        if grapheme.contains('\u{FE0F}') {
            vs16_columns
        } else {
            grapheme.width()
        }
    }

    /// Replays crossterm output the way a terminal does: `ESC [ row ; col H`
    /// moves the cursor, other escape sequences change no cells, and each
    /// grapheme is written at the cursor, which then moves past it. Writing
    /// over either half of a wide grapheme blanks the other half.
    fn replay(screen: &mut Screen, bytes: &[u8], vs16_columns: usize) {
        let mut rest = std::str::from_utf8(bytes).expect("crossterm writes UTF-8");
        let (mut x, mut y) = (0, 0);
        while !rest.is_empty() {
            if let Some(sequence) = rest.strip_prefix("\u{1b}[") {
                let end = sequence
                    .find(|c: char| ('@'..='~').contains(&c))
                    .expect("complete escape sequence");
                if sequence[end..].starts_with('H') {
                    let (row, col) = sequence[..end].split_once(';').expect("row;col");
                    y = row.parse::<usize>().unwrap() - 1;
                    x = col.parse::<usize>().unwrap() - 1;
                }
                rest = &sequence[end + 1..];
                continue;
            }
            let text_end = rest.find('\u{1b}').unwrap_or(rest.len());
            for grapheme in rest[..text_end].graphemes(true) {
                let width = columns(grapheme, vs16_columns);
                let row = &mut screen[y];
                let end = (x + width).min(row.len());
                for column in x..end {
                    if row[column].is_none() && column > 0 && column - 1 < x {
                        row[column - 1] = Some(" ".to_string());
                    }
                    if row[column].is_some()
                        && column + 1 >= end
                        && let Some(covered @ None) = row.get_mut(column + 1)
                    {
                        *covered = Some(" ".to_string());
                    }
                }
                if x < end {
                    row[x] = Some(grapheme.to_string());
                    row[x + 1..end].fill(None);
                }
                x += width;
            }
            rest = &rest[text_end..];
        }
    }

    /// The screen a terminal should show for `buffer`.
    fn shown(buffer: &Buffer, vs16_columns: usize) -> Screen {
        let area = buffer.area;
        (area.top()..area.bottom())
            .map(|y| {
                let mut row = Vec::new();
                while row.len() < usize::from(area.width) {
                    let symbol = buffer[(row.len() as u16, y)].symbol();
                    let width = columns(symbol, vs16_columns).max(1);
                    row.push(Some(symbol.to_string()));
                    row.extend(std::iter::repeat_n(None, width - 1));
                }
                row.truncate(usize::from(area.width));
                row
            })
            .collect()
    }

    #[test]
    fn capability_backend_keeps_a_row_aligned_after_a_vs16_emoji() {
        let area = Rect::new(0, 0, 10, 1);
        let rows = [
            ("aaaaaaaaaa", "\u{2764}\u{FE0F}bc"),
            ("│ abcdef │", "│ ok \u{26A0}\u{FE0F}Y │"),
            ("aaaaaaaaaa", "\u{2764}\u{FE0F}漢😀b"),
            ("漢字漢字漢", "a\u{2764}\u{FE0F}b"),
        ];

        for level in [TerminalColorLevel::TrueColor, TerminalColorLevel::Ansi16] {
            for vs16_columns in [2, 1] {
                for (before, after) in rows {
                    let blank = Buffer::empty(area);
                    let mut previous = Buffer::empty(area);
                    previous.set_string(0, 0, before, Style::default());
                    let mut next = Buffer::empty(area);
                    next.set_string(0, 0, after, Style::default());

                    let mut bytes = Vec::new();
                    {
                        let mut backend =
                            CapabilityBackend::new(CrosstermBackend::new(&mut bytes), level);
                        backend.draw(blank.diff(&previous).into_iter()).unwrap();
                        backend.draw(previous.diff(&next).into_iter()).unwrap();
                    }

                    let mut screen = vec![vec![Some(" ".to_string()); 10]];
                    replay(&mut screen, &bytes, vs16_columns);
                    assert_eq!(
                        screen,
                        shown(&next, vs16_columns),
                        "{level:?}, VS16 in {vs16_columns} columns: {before:?} -> {after:?}"
                    );
                }
            }
        }
    }
}
