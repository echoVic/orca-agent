#![cfg_attr(not(test), allow(dead_code))]

use std::ops::Range;

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

use crate::terminal_capabilities::TerminalColorLevel;

/// The character Kitty's unicode-placeholder image protocol puts in every cell
/// an image covers. Such a cell's foreground color is the image id rather than
/// a color to show, so it has to reach the terminal unchanged.
const KITTY_IMAGE_PLACEHOLDER: char = '\u{10EEEE}';

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
}

impl<B: Backend> Backend for CapabilityBackend<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        if self.color_level == TerminalColorLevel::TrueColor {
            return self.inner.draw(content);
        }

        let adapted = content
            .map(|(x, y, cell)| {
                let mut cell = cell.clone();
                let image_id = cell
                    .symbol()
                    .contains(KITTY_IMAGE_PLACEHOLDER)
                    .then_some(cell.fg);
                cell.set_style(self.color_level.adapt_style(cell.style()));
                if let Some(image_id) = image_id {
                    cell.fg = image_id;
                }
                (x, y, cell)
            })
            .collect::<Vec<_>>();
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io;
    use std::ops::Range;

    use ratatui::backend::{Backend, ClearType, WindowSize};
    use ratatui::buffer::{Cell, CellDiffOption};
    use ratatui::layout::{Position, Size};
    use ratatui::style::{Color, Modifier, Style};

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
}
