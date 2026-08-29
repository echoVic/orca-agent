use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
#[cfg(not(test))]
use std::sync::OnceLock;

use image::imageops::FilterType;
use image::{DynamicImage, Rgba, RgbaImage};
use qwertty::{TerminalIdentity, TerminalProgram};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, StatefulImage};
use unicode_width::UnicodeWidthStr;

use crate::composer_images::TuiImage;
use crate::display_text::truncate_to_display_width;
use crate::theme::Theme;

const THUMBNAIL_MAX_COLS: u16 = 40;
const THUMBNAIL_MAX_ROWS: u16 = 8;
const COMPOSER_PREVIEW_MAX_ROWS: u16 = 16;
const VIEWER_MIN_WIDTH: u16 = 28;
const VIEWER_MIN_HEIGHT: u16 = 10;
const VIEWER_MAX_ZOOM: u16 = 800;
const VIEWER_ZOOM_STEP: u16 = 25;
const DEFAULT_CELL_PIXEL_SIZE: (u16, u16) = (10, 20);
pub(crate) const VIEWER_PAN_STEP: i16 = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeImageProtocol {
    None,
    Kitty,
    Iterm2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ImageRenderProfile {
    protocol: NativeImageProtocol,
    cell_size: (u16, u16),
}

impl ImageRenderProfile {
    #[cfg(not(test))]
    pub(crate) fn detect() -> Self {
        let identity = qwertty::caps::identity_from_env(None, qwertty::caps::std_env_source);
        Self::from_identity(&identity, terminal_cell_pixel_size())
    }

    fn from_identity(identity: &TerminalIdentity, cell_size: (u16, u16)) -> Self {
        let inside_multiplexer = !identity.mux_stack.is_empty();
        let protocol = if inside_multiplexer {
            NativeImageProtocol::None
        } else {
            match identity.program.as_ref() {
                Some(TerminalProgram::Kitty | TerminalProgram::Ghostty) => {
                    NativeImageProtocol::Kitty
                }
                Some(TerminalProgram::Iterm2 | TerminalProgram::WezTerm) => {
                    NativeImageProtocol::Iterm2
                }
                _ => NativeImageProtocol::None,
            }
        };
        Self {
            protocol,
            cell_size,
        }
    }
}

#[cfg(not(test))]
fn detected_image_render_profile() -> ImageRenderProfile {
    static PROFILE: OnceLock<ImageRenderProfile> = OnceLock::new();
    *PROFILE.get_or_init(ImageRenderProfile::detect)
}

#[cfg(test)]
fn native_image_rendering_active() -> bool {
    false
}

#[cfg(not(test))]
fn native_image_rendering_active() -> bool {
    detected_image_render_profile().protocol != NativeImageProtocol::None
}

#[cfg(not(test))]
fn terminal_cell_pixel_size() -> (u16, u16) {
    crossterm::terminal::window_size()
        .ok()
        .and_then(|window| {
            (window.columns > 0 && window.rows > 0 && window.width > 0 && window.height > 0).then(
                || {
                    (
                        window.width.div_ceil(window.columns).max(1),
                        window.height.div_ceil(window.rows).max(1),
                    )
                },
            )
        })
        .unwrap_or(DEFAULT_CELL_PIXEL_SIZE)
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ImageRenderSurface {
    Transcript,
    Composer,
    Viewer,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct NativeImageKey {
    source: usize,
    encoded_len: usize,
    surface: ImageRenderSurface,
    zoom_percent: u16,
    pan_x: i16,
    pan_y: i16,
}

pub(crate) struct ImageRenderState {
    profile: ImageRenderProfile,
    picker: Option<Picker>,
    cache: HashMap<NativeImageKey, Option<StatefulProtocol>>,
}

impl fmt::Debug for ImageRenderState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImageRenderState")
            .field("profile", &self.profile)
            .field("cached_images", &self.cache.len())
            .finish()
    }
}

impl Default for ImageRenderState {
    fn default() -> Self {
        #[cfg(test)]
        let profile = ImageRenderProfile {
            protocol: NativeImageProtocol::None,
            cell_size: DEFAULT_CELL_PIXEL_SIZE,
        };
        #[cfg(not(test))]
        let profile = detected_image_render_profile();
        Self::new(profile)
    }
}

impl ImageRenderState {
    fn new(profile: ImageRenderProfile) -> Self {
        let picker = match profile.protocol {
            NativeImageProtocol::None => None,
            protocol => {
                #[allow(deprecated)]
                let mut picker = Picker::from_fontsize(profile.cell_size);
                picker.set_protocol_type(match protocol {
                    NativeImageProtocol::Kitty => ProtocolType::Kitty,
                    NativeImageProtocol::Iterm2 => ProtocolType::Iterm2,
                    NativeImageProtocol::None => unreachable!(),
                });
                Some(picker)
            }
        };
        Self {
            profile,
            picker,
            cache: HashMap::new(),
        }
    }

    fn supports_native_images(&self) -> bool {
        self.picker.is_some()
    }

    pub(crate) fn paint_native(
        &mut self,
        frame: &mut Frame,
        image: &TuiImage,
        area: Rect,
        surface: ImageRenderSurface,
        zoom_percent: u16,
        pan_x: i16,
        pan_y: i16,
    ) -> bool {
        if area.is_empty() {
            return false;
        }
        let Some(picker) = self.picker.as_ref() else {
            return false;
        };
        let Some(bytes) = image.encoded.as_ref() else {
            return false;
        };
        let key = NativeImageKey {
            source: bytes.as_ptr() as usize,
            encoded_len: bytes.len(),
            surface,
            zoom_percent,
            pan_x,
            pan_y,
        };
        let protocol = self.cache.entry(key).or_insert_with(|| {
            decode_view(image, zoom_percent, pan_x, pan_y)
                .map(|decoded| picker.new_resize_protocol(decoded))
        });
        let Some(protocol) = protocol.as_mut() else {
            return false;
        };
        let resize = Resize::Fit(Some(ratatui_image::FilterType::Lanczos3));
        let fitted = protocol.size_for(resize.clone(), area);
        let target = Rect::new(
            area.x + area.width.saturating_sub(fitted.width) / 2,
            area.y + area.height.saturating_sub(fitted.height) / 2,
            fitted.width,
            fitted.height,
        );
        frame.render_widget(Clear, area);
        frame.render_stateful_widget(StatefulImage::new().resize(resize), target, protocol);
        protocol
            .last_encoding_result()
            .is_none_or(|result| result.is_ok())
    }
}

#[derive(Clone)]
pub(crate) struct ImageViewerState {
    image: TuiImage,
    decoded: Arc<RgbaImage>,
    zoom_percent: u16,
    pan_x: i16,
    pan_y: i16,
}

impl fmt::Debug for ImageViewerState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImageViewerState")
            .field("image", &self.image)
            .field("zoom_percent", &self.zoom_percent)
            .field("pan_x", &self.pan_x)
            .field("pan_y", &self.pan_y)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ImageHitArea {
    pub(crate) rect: Rect,
    pub(crate) preview_rect: Option<Rect>,
    pub(crate) image: TuiImage,
}

impl ImageViewerState {
    pub(crate) fn open(image: TuiImage) -> Result<Self, String> {
        let bytes = image
            .encoded
            .as_deref()
            .ok_or_else(|| "this image source has no local preview bytes".to_string())?;
        let pixels = u64::from(image.width).saturating_mul(u64::from(image.height));
        if pixels == 0 || pixels > crate::clipboard_image::MAX_COMPOSER_IMAGE_PIXELS {
            return Err("image dimensions are unavailable or exceed the preview limit".to_string());
        }
        let decoded = image::load_from_memory(bytes)
            .map_err(|error| format!("failed to decode image preview: {error}"))?
            .to_rgba8();
        Ok(Self {
            image,
            decoded: Arc::new(decoded),
            zoom_percent: 100,
            pan_x: 0,
            pan_y: 0,
        })
    }

    pub(crate) fn zoom_in(&mut self) {
        self.zoom_percent = self
            .zoom_percent
            .saturating_add(VIEWER_ZOOM_STEP)
            .min(VIEWER_MAX_ZOOM);
    }

    pub(crate) fn zoom_out(&mut self) {
        self.zoom_percent = self.zoom_percent.saturating_sub(VIEWER_ZOOM_STEP).max(100);
        if self.zoom_percent == 100 {
            self.pan_x = 0;
            self.pan_y = 0;
        }
    }

    pub(crate) fn reset_view(&mut self) {
        self.zoom_percent = 100;
        self.pan_x = 0;
        self.pan_y = 0;
    }

    pub(crate) fn pan(&mut self, dx: i16, dy: i16) {
        if self.zoom_percent == 100 {
            return;
        }
        self.pan_x = self.pan_x.saturating_add(dx).clamp(-100, 100);
        self.pan_y = self.pan_y.saturating_add(dy).clamp(-100, 100);
    }
}

pub(crate) fn thumbnail_lines(
    image: &TuiImage,
    available_width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if native_image_rendering_active() {
        return native_preview_lines(image, available_width, THUMBNAIL_MAX_ROWS, theme);
    }
    preview_lines(image, available_width, THUMBNAIL_MAX_ROWS, theme)
}

fn native_preview_lines(
    image: &TuiImage,
    available_width: usize,
    max_rows: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let width = available_width.min(u16::MAX as usize) as u16;
    let rows = preview_rows_for_bounds(image, width, max_rows);
    if width < 8 || rows == 0 {
        return preview_lines(image, available_width, max_rows, theme);
    }
    let metadata = image_metadata(image);
    let title =
        truncate_to_display_width(&format!("  {}  {}", image.label, metadata), width as usize);
    let mut lines = Vec::with_capacity(usize::from(rows) + 2);
    lines.push(Line::from(Span::styled(
        title,
        Style::default().fg(theme.user).add_modifier(Modifier::BOLD),
    )));
    lines.extend((0..rows).map(|_| Line::from("")));
    lines.push(Line::from(""));
    lines
}

fn preview_lines(
    image: &TuiImage,
    available_width: usize,
    max_rows: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let width = available_width.min(u16::MAX as usize) as u16;
    if width < 8 {
        return vec![Line::from(Span::styled(
            image.label.clone(),
            Style::default().fg(theme.user),
        ))];
    }
    let metadata = image_metadata(image);
    let title =
        truncate_to_display_width(&format!("  {}  {}", image.label, metadata), width as usize);
    let mut lines = vec![Line::from(Span::styled(
        title,
        Style::default().fg(theme.user).add_modifier(Modifier::BOLD),
    ))];
    let max_cols = width.saturating_sub(4).min(THUMBNAIL_MAX_COLS);
    let pixels = u64::from(image.width).saturating_mul(u64::from(image.height));
    if pixels == 0 || pixels > crate::clipboard_image::MAX_COMPOSER_IMAGE_PIXELS {
        lines.push(Line::from(Span::styled(
            "  preview unavailable",
            Style::default().fg(theme.muted),
        )));
        lines.push(Line::from(""));
        return lines;
    }
    let Some(bytes) = image.encoded.as_deref() else {
        lines.push(Line::from(Span::styled(
            "  preview unavailable",
            Style::default().fg(theme.muted),
        )));
        lines.push(Line::from(""));
        return lines;
    };
    let Ok(decoded) = image::load_from_memory(bytes) else {
        lines.push(Line::from(Span::styled(
            "  preview unavailable",
            Style::default().fg(theme.muted),
        )));
        lines.push(Line::from(""));
        return lines;
    };
    lines.extend(raster_lines(&decoded, max_cols, max_rows, 100, 0, 0, 2));
    lines.push(Line::from(""));
    lines
}

pub(crate) fn render_viewer(
    frame: &mut Frame,
    state: &ImageViewerState,
    image_renderer: &mut ImageRenderState,
    theme: &Theme,
) -> Option<Rect> {
    let area = frame.area();
    if area.width < VIEWER_MIN_WIDTH || area.height < VIEWER_MIN_HEIGHT {
        return None;
    }
    let width = ((u32::from(area.width) * 9) / 10)
        .max(u32::from(VIEWER_MIN_WIDTH))
        .min(u32::from(area.width)) as u16;
    let height = ((u32::from(area.height) * 9) / 10)
        .max(u32::from(VIEWER_MIN_HEIGHT))
        .min(u32::from(area.height)) as u16;
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let title = state
        .image
        .source_name
        .as_deref()
        .unwrap_or(state.image.label.as_str());
    let title = sanitize_single_line(title);
    let suffix = format!(
        " · {}% · {} ",
        state.zoom_percent,
        image_metadata(&state.image)
    );
    let suffix_width = UnicodeWidthStr::width(suffix.as_str()).min(u16::MAX as usize) as u16;
    let title = format!(
        " {}{}",
        truncate_to_display_width(
            &title,
            width.saturating_sub(2).saturating_sub(suffix_width) as usize,
        ),
        suffix
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(Color::Reset));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.is_empty() {
        return Some(popup);
    }
    if !image_renderer.supports_native_images() {
        let lines = raster_lines_from_rgba(
            state.decoded.as_ref(),
            inner.width,
            inner.height,
            state.zoom_percent,
            state.pan_x,
            state.pan_y,
            0,
        );
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Color::Reset)),
            inner,
        );
    }
    image_renderer.paint_native(
        frame,
        &state.image,
        inner,
        ImageRenderSurface::Viewer,
        state.zoom_percent,
        state.pan_x,
        state.pan_y,
    );
    Some(popup)
}

pub(crate) fn render_composer_preview(
    frame: &mut Frame,
    area: Rect,
    image: &TuiImage,
    image_renderer: &mut ImageRenderState,
    theme: &Theme,
) -> Option<Rect> {
    if area.width < 16 || area.height < 5 {
        return None;
    }
    let width = area.width.min(48);
    let preview_rows = area.height.saturating_sub(2).min(COMPOSER_PREVIEW_MAX_ROWS);
    let mut lines = if image_renderer.supports_native_images() {
        native_preview_lines(
            image,
            width.saturating_sub(2) as usize,
            preview_rows.saturating_sub(1),
            theme,
        )
    } else {
        preview_lines(
            image,
            width.saturating_sub(2) as usize,
            preview_rows.saturating_sub(1),
            theme,
        )
    };
    if lines.last().is_some_and(|line| line.spans.is_empty()) {
        lines.pop();
    }
    lines.truncate(preview_rows as usize);
    let height = (lines.len() as u16).saturating_add(2).min(area.height);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height),
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border));
    let inner = block.inner(popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
    let image_area = Rect::new(
        inner.x.saturating_add(2),
        inner.y.saturating_add(1),
        inner.width.saturating_sub(2),
        inner.height.saturating_sub(1),
    );
    image_renderer.paint_native(
        frame,
        image,
        image_area,
        ImageRenderSurface::Composer,
        100,
        0,
        0,
    );
    Some(popup)
}

fn image_metadata(image: &TuiImage) -> String {
    let dimensions = if image.width > 0 && image.height > 0 {
        format!("{}x{}", image.width, image.height)
    } else {
        "unknown size".to_string()
    };
    let format = sanitize_single_line(
        image
            .media_type
            .strip_prefix("image/")
            .unwrap_or(image.media_type.as_str()),
    );
    format!("{dimensions} · {format}")
}

fn sanitize_single_line(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn raster_lines(
    image: &DynamicImage,
    max_cols: u16,
    max_rows: u16,
    zoom_percent: u16,
    pan_x: i16,
    pan_y: i16,
    indent: usize,
) -> Vec<Line<'static>> {
    raster_lines_from_rgba(
        &image.to_rgba8(),
        max_cols,
        max_rows,
        zoom_percent,
        pan_x,
        pan_y,
        indent,
    )
}

fn raster_lines_from_rgba(
    image: &RgbaImage,
    max_cols: u16,
    max_rows: u16,
    zoom_percent: u16,
    pan_x: i16,
    pan_y: i16,
    indent: usize,
) -> Vec<Line<'static>> {
    if max_cols == 0 || max_rows == 0 || image.width() == 0 || image.height() == 0 {
        return Vec::new();
    }
    let zoom = u32::from(zoom_percent.max(100));
    let crop_width = image.width().saturating_mul(100).div_ceil(zoom).max(1);
    let crop_height = image.height().saturating_mul(100).div_ceil(zoom).max(1);
    let max_x = image.width().saturating_sub(crop_width);
    let max_y = image.height().saturating_sub(crop_height);
    let x = pan_offset(max_x, pan_x);
    let y = pan_offset(max_y, pan_y);
    let crop = image::imageops::crop_imm(image, x, y, crop_width, crop_height).to_image();

    let (target_width, target_height) =
        fitted_raster_dimensions(crop.width(), crop.height(), max_cols, max_rows);
    let resized = image::imageops::resize(&crop, target_width, target_height, FilterType::Lanczos3);

    let mut lines = Vec::with_capacity(target_height.div_ceil(2) as usize);
    for y in (0..target_height).step_by(2) {
        let mut spans = Vec::with_capacity(target_width as usize + usize::from(indent > 0));
        if indent > 0 {
            spans.push(Span::raw(" ".repeat(indent)));
        }
        for x in 0..target_width {
            let top = flatten(*resized.get_pixel(x, y));
            let bottom = if y + 1 < target_height {
                flatten(*resized.get_pixel(x, y + 1))
            } else {
                top
            };
            spans.push(Span::styled(
                "▀",
                Style::default()
                    .fg(Color::Rgb(top.0, top.1, top.2))
                    .bg(Color::Rgb(bottom.0, bottom.1, bottom.2)),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}

pub(crate) fn thumbnail_preview_rows(image: &TuiImage, available_width: usize) -> u16 {
    let width = available_width.min(u16::MAX as usize) as u16;
    preview_rows_for_bounds(image, width, THUMBNAIL_MAX_ROWS)
}

fn preview_rows_for_bounds(image: &TuiImage, width: u16, max_rows: u16) -> u16 {
    let max_cols = width.saturating_sub(4).min(THUMBNAIL_MAX_COLS);
    if max_cols == 0
        || image.width == 0
        || image.height == 0
        || image.encoded.is_none()
        || u64::from(image.width).saturating_mul(u64::from(image.height))
            > crate::clipboard_image::MAX_COMPOSER_IMAGE_PIXELS
    {
        return 0;
    }
    let (_, pixel_height) = fitted_raster_dimensions(image.width, image.height, max_cols, max_rows);
    pixel_height.div_ceil(2) as u16
}

fn fitted_raster_dimensions(width: u32, height: u32, max_cols: u16, max_rows: u16) -> (u32, u32) {
    let max_pixel_height = u32::from(max_rows).saturating_mul(2);
    let scale = (f64::from(max_cols) / f64::from(width))
        .min(f64::from(max_pixel_height) / f64::from(height));
    (
        ((f64::from(width) * scale).round() as u32).clamp(1, u32::from(max_cols)),
        ((f64::from(height) * scale).round() as u32).clamp(1, max_pixel_height),
    )
}

fn decode_view(
    image: &TuiImage,
    zoom_percent: u16,
    pan_x: i16,
    pan_y: i16,
) -> Option<DynamicImage> {
    let bytes = image.encoded.as_deref()?;
    let decoded = image::load_from_memory(bytes).ok()?.to_rgba8();
    if decoded.width() == 0 || decoded.height() == 0 {
        return None;
    }
    let zoom = u32::from(zoom_percent.max(100));
    let crop_width = decoded.width().saturating_mul(100).div_ceil(zoom).max(1);
    let crop_height = decoded.height().saturating_mul(100).div_ceil(zoom).max(1);
    let max_x = decoded.width().saturating_sub(crop_width);
    let max_y = decoded.height().saturating_sub(crop_height);
    let x = pan_offset(max_x, pan_x);
    let y = pan_offset(max_y, pan_y);
    Some(DynamicImage::ImageRgba8(
        image::imageops::crop_imm(&decoded, x, y, crop_width, crop_height).to_image(),
    ))
}

fn pan_offset(max_offset: u32, pan: i16) -> u32 {
    let normalized = i32::from(pan.clamp(-100, 100)) + 100;
    (u64::from(max_offset) * normalized as u64 / 200) as u32
}

fn flatten(pixel: Rgba<u8>) -> (u8, u8, u8) {
    let alpha = u16::from(pixel[3]);
    let background = 24u16;
    let blend =
        |channel: u8| ((u16::from(channel) * alpha + background * (255 - alpha)) / 255) as u8;
    (blend(pixel[0]), blend(pixel[1]), blend(pixel[2]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder as _;

    fn image() -> TuiImage {
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(
                &[255, 0, 0, 255, 0, 255, 0, 255],
                2,
                1,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        TuiImage {
            label: "[Image #1]".to_string(),
            media_type: "image/png".to_string(),
            encoded: Some(Arc::from(bytes)),
            width: 2,
            height: 1,
            source_name: Some("sample.png".to_string()),
        }
    }

    fn identity(values: &[(&str, &str)]) -> TerminalIdentity {
        qwertty::caps::identity_from_env(None, |key| {
            values
                .iter()
                .find_map(|(candidate, value)| (*candidate == key).then(|| (*value).to_string()))
        })
    }

    #[test]
    fn native_image_profile_only_enables_supported_terminals() {
        assert_eq!(
            ImageRenderProfile::from_identity(&identity(&[("TERM_PROGRAM", "ghostty")]), (12, 24),),
            ImageRenderProfile {
                protocol: NativeImageProtocol::Kitty,
                cell_size: (12, 24),
            }
        );
        assert_eq!(
            ImageRenderProfile::from_identity(
                &identity(&[("TERM_PROGRAM", "iTerm.app")]),
                (12, 24),
            ),
            ImageRenderProfile {
                protocol: NativeImageProtocol::Iterm2,
                cell_size: (12, 24),
            }
        );
        assert_eq!(
            ImageRenderProfile::from_identity(
                &identity(&[("TERM_PROGRAM", "Apple_Terminal")]),
                (12, 24),
            )
            .protocol,
            NativeImageProtocol::None
        );
    }

    #[test]
    fn native_preview_reserves_exactly_one_visual_row_per_image_row() {
        let image = image();
        let rows = thumbnail_preview_rows(&image, 80);
        let lines = native_preview_lines(
            &image,
            80,
            THUMBNAIL_MAX_ROWS,
            &Theme::named(orca_core::config::ThemeName::Dark),
        );

        assert_eq!(lines.len(), usize::from(rows) + 2);
        assert!(
            lines[1..=usize::from(rows)]
                .iter()
                .all(|line| line.width() == 0)
        );
    }

    #[test]
    fn kitty_renderer_emits_native_graphics_sequence() {
        let mut renderer = ImageRenderState::new(ImageRenderProfile {
            protocol: NativeImageProtocol::Kitty,
            cell_size: (10, 20),
        });
        assert!(renderer.supports_native_images());
        let backend = ratatui::backend::TestBackend::new(40, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new(vec![Line::from("▀".repeat(20)); 10]),
                    Rect::new(2, 2, 20, 10),
                );
                assert!(renderer.paint_native(
                    frame,
                    &image(),
                    Rect::new(2, 2, 20, 10),
                    ImageRenderSurface::Viewer,
                    100,
                    0,
                    0,
                ));
            })
            .unwrap();

        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .any(|cell| cell.symbol().contains("\u{10eeee}"))
        );
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .all(|cell| !cell.symbol().contains('▀'))
        );
    }

    #[test]
    fn thumbnail_contains_metadata_and_colored_pixels() {
        let lines = thumbnail_lines(
            &image(),
            80,
            &Theme::named(orca_core::config::ThemeName::Dark),
        );
        assert!(lines[0].to_string().contains("[Image #1]"));
        assert!(lines[0].to_string().contains("2x1"));
        assert!(lines.iter().any(|line| line.to_string().contains('▀')));
    }

    #[test]
    fn viewer_zoom_and_pan_are_bounded() {
        let mut viewer = ImageViewerState::open(image()).unwrap();
        for _ in 0..100 {
            viewer.zoom_in();
            viewer.pan(VIEWER_PAN_STEP, -VIEWER_PAN_STEP);
        }
        assert_eq!(viewer.zoom_percent, VIEWER_MAX_ZOOM);
        assert_eq!((viewer.pan_x, viewer.pan_y), (100, -100));
        viewer.reset_view();
        assert_eq!(
            (viewer.zoom_percent, viewer.pan_x, viewer.pan_y),
            (100, 0, 0)
        );
    }

    #[test]
    fn message_thumbnail_registers_a_clickable_hit_area() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut state = crate::types::AppState::new(
            tx,
            "test".to_string(),
            "auto".to_string(),
            "/tmp".to_string(),
        );
        state.push_message(crate::transcript_state::ChatMessage::User(
            "inspect [Image #1]".to_string(),
        ));
        state.push_message(crate::transcript_state::ChatMessage::Image(image()));
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let textarea = tui_textarea::TextArea::default();
        let backend = ratatui::backend::TestBackend::new(80, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| crate::ui::render(frame, &mut state, &textarea, &theme))
            .unwrap();

        assert_eq!(state.image_hit_areas.len(), 1);
        let hit = state.image_hit_areas[0].rect;
        let event = crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: hit.x,
            row: hit.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(
            crate::input_event_actions::handle_mouse_event(
                &event,
                &mut state,
                &mut tui_textarea::TextArea::default(),
                std::time::Instant::now(),
            ),
            crate::input_event_actions::MouseFlow::Handled
        );
        assert!(state.image_viewer.is_some());
    }
}
