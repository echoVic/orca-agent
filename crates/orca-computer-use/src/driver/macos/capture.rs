//! ScreenCaptureKit snapshots contain plain data; native objects stay inside
//! completion callbacks rather than crossing the driver's channel.
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::{AllocAnyThread, rc::autoreleasepool};
use objc2_core_foundation::{CFBoolean, CFString, CFType, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGSessionCopyCurrentDictionary,
};
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration,
};

use super::error;
use crate::{
    geometry,
    protocol::{DriverError, ErrorKind, Screenshot},
};

#[derive(Clone, Debug)]
pub(super) struct Window {
    pub id: u32,
    pub pid: i32,
    pub title: String,
    pub frame: [f64; 4],
    pub scale: f64,
    pub on_screen: bool,
}

pub(super) fn check_unlocked() -> Result<(), DriverError> {
    if let Some(session) = CGSessionCopyCurrentDictionary() {
        let key = CFString::from_str("CGSSessionScreenIsLocked");
        let value = unsafe { session.value((&*key as *const CFString).cast()) };
        if !value.is_null() {
            let value = unsafe { &*value.cast::<CFType>() };
            if value.downcast_ref::<CFBoolean>().is_some_and(|v| v.value()) {
                return Err(error(ErrorKind::ScreenLocked, "The screen is locked"));
            }
        }
    }
    Ok(())
}

pub(super) fn windows() -> Result<Vec<Window>, DriverError> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let completion = RcBlock::new(
        move |content: *mut SCShareableContent, failure: *mut NSError| {
            let result = autoreleasepool(|_| unsafe {
                if let Some(failure) = failure.as_ref() {
                    return Err(error(
                        ErrorKind::Failed,
                        format!("Could not list windows: {}", failure.localizedDescription()),
                    ));
                }
                let content = content.as_ref().ok_or_else(|| {
                    error(ErrorKind::Failed, "Window listing returned no content")
                })?;
                Ok(content
                    .windows()
                    .iter()
                    .filter_map(|window| {
                        let app = window.owningApplication()?;
                        let frame = window.frame();
                        let filter = SCContentFilter::initWithDesktopIndependentWindow(
                            SCContentFilter::alloc(),
                            &window,
                        );
                        Some(Window {
                            id: window.windowID(),
                            pid: app.processID(),
                            title: window.title().map(|s| s.to_string()).unwrap_or_default(),
                            frame: [
                                frame.origin.x,
                                frame.origin.y,
                                frame.size.width,
                                frame.size.height,
                            ],
                            // This is the window display's backing scale, as reported by SCK.
                            scale: f64::from(filter.pointPixelScale()),
                            on_screen: window.isOnScreen(),
                        })
                    })
                    .collect())
            });
            let _ = sender.send(result);
        },
    );
    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&completion);
    }
    receiver
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| error(ErrorKind::Failed, "Window listing timed out"))?
}

fn rgba(image: &CGImage) -> Result<image::RgbaImage, DriverError> {
    let width = CGImage::width(Some(image));
    let height = CGImage::height(Some(image));
    let mut pixels = image::RgbaImage::new(width as u32, height as u32);
    let color = CGColorSpace::new_device_rgb()
        .ok_or_else(|| error(ErrorKind::Failed, "Could not create screenshot color space"))?;
    // Big-endian RGBA with premultiplied alpha; the context borrows pixels
    // until it is dropped, before we return the buffer.
    let context = unsafe {
        CGBitmapContextCreate(
            pixels.as_mut_ptr().cast(),
            width,
            height,
            8,
            width * 4,
            Some(&color),
            (4 << 12) | 1,
        )
    }
    .ok_or_else(|| error(ErrorKind::Failed, "Could not create screenshot bitmap"))?;
    CGContext::draw_image(
        Some(&context),
        CGRect {
            origin: CGPoint::default(),
            size: CGSize {
                width: width as f64,
                height: height as f64,
            },
        },
        Some(image),
    );
    drop(context);
    for pixel in pixels.pixels_mut() {
        let alpha = u32::from(pixel[3]);
        if alpha > 0 && alpha < 255 {
            for channel in &mut pixel.0[..3] {
                *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
    }
    Ok(pixels)
}

pub(super) fn screenshot(window: &Window) -> Result<Screenshot, DriverError> {
    check_unlocked()?;
    let (sender, receiver) = mpsc::sync_channel(1);
    let id = window.id;
    let pid = window.pid;
    let width = (window.frame[2] * window.scale).round().max(1.0) as usize;
    let height = (window.frame[3] * window.scale).round().max(1.0) as usize;
    let completion = RcBlock::new(
        move |content: *mut SCShareableContent, failure: *mut NSError| {
            autoreleasepool(|_| unsafe {
                let setup = (|| {
                    if let Some(failure) = failure.as_ref() {
                        return Err(error(
                            ErrorKind::Failed,
                            format!(
                                "Could not list capture windows: {}",
                                failure.localizedDescription()
                            ),
                        ));
                    }
                    let content = content
                        .as_ref()
                        .ok_or_else(|| error(ErrorKind::Failed, "Capture returned no content"))?;
                    let window = content
                        .windows()
                        .iter()
                        .find(|w| {
                            w.windowID() == id
                                && w.owningApplication().is_some_and(|a| a.processID() == pid)
                        })
                        .ok_or_else(|| {
                            error(
                                ErrorKind::WindowNotFound,
                                "The capture window is no longer available",
                            )
                        })?;
                    let filter = SCContentFilter::initWithDesktopIndependentWindow(
                        SCContentFilter::alloc(),
                        &window,
                    );
                    let config = SCStreamConfiguration::new();
                    config.setWidth(width);
                    config.setHeight(height);
                    config.setShowsCursor(false);
                    config.setIgnoreShadowsSingleWindow(true);
                    let image_sender = sender.clone();
                    let image_completion =
                        RcBlock::new(move |image: *mut CGImage, failure: *mut NSError| {
                            let result = autoreleasepool(|_| {
                                if let Some(failure) = failure.as_ref() {
                                    return Err(error(
                                        ErrorKind::Failed,
                                        format!(
                                            "Could not capture window: {}",
                                            failure.localizedDescription()
                                        ),
                                    ));
                                }
                                let image = image.as_ref().ok_or_else(|| {
                                    error(ErrorKind::Failed, "Screenshot returned no image")
                                })?;
                                rgba(image)
                            });
                            let _ = image_sender.send(result);
                        });
                    SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
                        &filter,
                        &config,
                        Some(&image_completion),
                    );
                    Ok(())
                })();
                if let Err(failure) = setup {
                    let _ = sender.send(Err(failure));
                }
            });
        },
    );
    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&completion);
    }
    let pixels = receiver
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| error(ErrorKind::Failed, "Window capture timed out"))??;
    let (width, height) = geometry::screenshot_size(pixels.width(), pixels.height());
    Ok(Screenshot {
        width,
        height,
        png_base64: geometry::encode_png(&pixels)
            .map_err(|message| error(ErrorKind::Failed, message))?,
    })
}
