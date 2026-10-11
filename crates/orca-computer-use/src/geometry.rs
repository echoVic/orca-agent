use base64::Engine;
use image::ImageEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;

pub const MAX_SCREENSHOT_SIDE: u32 = 1280;

/// Never scales up; the shorter side rounds to the nearest integer.
pub fn screenshot_size(width: u32, height: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= MAX_SCREENSHOT_SIDE {
        return (width, height);
    }
    let scale = f64::from(MAX_SCREENSHOT_SIDE) / f64::from(longest);
    (
        (f64::from(width) * scale).round() as u32,
        (f64::from(height) * scale).round() as u32,
    )
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CaptureGeometry {
    pub window_origin: (f64, f64),
    pub window_size: (f64, f64),
    pub image_size: (u32, u32),
}

impl CaptureGeometry {
    pub fn to_image(&self, frame_pt: [f64; 4]) -> [i32; 4] {
        let scale_x = f64::from(self.image_size.0) / self.window_size.0;
        let scale_y = f64::from(self.image_size.1) / self.window_size.1;
        [
            ((frame_pt[0] - self.window_origin.0) * scale_x).round() as i32,
            ((frame_pt[1] - self.window_origin.1) * scale_y).round() as i32,
            (frame_pt[2] * scale_x).round() as i32,
            (frame_pt[3] * scale_y).round() as i32,
        ]
    }

    /// Returns None outside the image, or when the capture geometry is invalid.
    pub fn to_screen(&self, point_px: [f64; 2]) -> Option<(f64, f64)> {
        let [x, y] = point_px;
        if !x.is_finite()
            || !y.is_finite()
            || x < 0.0
            || y < 0.0
            || x >= f64::from(self.image_size.0)
            || y >= f64::from(self.image_size.1)
            || !self.window_size.0.is_finite()
            || !self.window_size.1.is_finite()
            || self.window_size.0 <= 0.0
            || self.window_size.1 <= 0.0
            || !self.window_origin.0.is_finite()
            || !self.window_origin.1.is_finite()
        {
            return None;
        }
        Some((
            self.window_origin.0 + x * self.window_size.0 / f64::from(self.image_size.0),
            self.window_origin.1 + y * self.window_size.1 / f64::from(self.image_size.1),
        ))
    }
}

pub fn encode_png(rgba: &image::RgbaImage) -> Result<String, String> {
    let (width, height) = screenshot_size(rgba.width(), rgba.height());
    if width == 0 || height == 0 {
        return Err("screenshot dimensions must be non-zero".into());
    }
    let resized;
    let image = if (width, height) == rgba.dimensions() {
        rgba
    } else {
        resized = image::imageops::resize(rgba, width, height, FilterType::Triangle);
        &resized
    };
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(
            image.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|error| format!("failed to encode screenshot PNG: {error}"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(png))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn screenshots_are_only_scaled_down() {
        assert_eq!(MAX_SCREENSHOT_SIDE, 1280);
        for (input, expected) in [
            ((2440, 1720), (1280, 902)),
            ((1720, 2440), (902, 1280)),
            ((460, 816), (460, 816)),
            ((1280, 1280), (1280, 1280)),
            ((0, 0), (0, 0)),
        ] {
            assert_eq!(screenshot_size(input.0, input.1), expected);
        }
    }

    #[test]
    fn points_map_back_to_screen_coordinates() {
        let geometry = CaptureGeometry {
            window_origin: (100.0, 50.0),
            window_size: (1220.0, 860.0),
            image_size: (1280, 902),
        };
        let (x, y) = geometry.to_screen([640.0, 451.0]).unwrap();
        assert!(
            (x - 710.0).abs() < 0.6 && (y - 480.0).abs() < 0.6,
            "{x},{y}"
        );
        assert_eq!(geometry.to_screen([0.0, 0.0]), Some((100.0, 50.0)));
        for point in [
            [1300.0, 10.0],
            [-1.0, 0.0],
            [1280.0, 0.0],
            [0.0, 902.0],
            [f64::NAN, 1.0],
            [0.0, f64::INFINITY],
        ] {
            assert_eq!(geometry.to_screen(point), None);
        }
        assert_eq!(
            geometry.to_image([710.0, 480.0, 610.0, 430.0]),
            [640, 451, 640, 451]
        );
    }

    #[test]
    fn invalid_capture_sizes_do_not_map_points() {
        for geometry in [
            CaptureGeometry {
                window_origin: (0.0, 0.0),
                window_size: (0.0, 10.0),
                image_size: (10, 10),
            },
            CaptureGeometry {
                window_origin: (0.0, 0.0),
                window_size: (10.0, 10.0),
                image_size: (0, 10),
            },
        ] {
            assert_eq!(geometry.to_screen([0.0, 0.0]), None);
        }
    }

    #[test]
    fn png_encoding_resizes_and_preserves_rgba_pixels() {
        for (size, expected) in [((2440, 1720), (1280, 902)), ((4, 3), (4, 3))] {
            let rgba = image::RgbaImage::from_pixel(size.0, size.1, image::Rgba([12, 34, 56, 78]));
            let encoded = encode_png(&rgba).unwrap();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap();
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
            let decoded = image::load_from_memory(&bytes).unwrap().to_rgba8();
            assert_eq!(decoded.dimensions(), expected);
            assert_eq!(decoded.get_pixel(0, 0), &image::Rgba([12, 34, 56, 78]));
        }
        assert!(encode_png(&image::RgbaImage::new(0, 0)).is_err());
    }
}
