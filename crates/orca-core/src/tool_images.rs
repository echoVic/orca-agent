//! Validation and budgeting for images that tool results carry.
//!
//! A tool (for example a computer-use screenshot) can attach images to its
//! `ToolResult`, which `Conversation::add_tool_result_with_terminal` copies
//! onto the resulting `Message::Tool`. This module holds the pure, provider-
//! and runtime-agnostic pieces of that channel: which media types and sizes
//! are acceptable, and the two ways a caller trims images back out of a
//! message list once they are no longer welcome.

use base64::Engine as _;

use crate::conversation::{ImageDetail, ImageInput, ImageSource, Message};

/// Image types accepted from tool results, detected from their file headers.
pub const TOOL_IMAGE_MEDIA_TYPES: [&str; 4] =
    ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Largest decoded size, in bytes, a single tool image may have.
///
/// Mirrors `MAX_INLINE_IMAGE_BYTES` in `orca-runtime`'s `mentions.rs`, which
/// applies the same limit to images a user attaches by mention.
pub const MAX_TOOL_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Appended to a tool message's content when its images were dropped because
/// they are not reloaded: in a resumed session, or in the restored
/// conversation of a continued child agent.
pub const RESUMED_TOOL_IMAGE_NOTE: &str = "[image omitted: not kept across session resume]";

/// Appended to a tool message's content when its images were dropped because
/// the model the request goes to cannot take them: it cannot see images at
/// all, or it is not known to accept images inside tool messages. It also asks
/// the model not to hunt for the image some other way.
pub const TOOL_IMAGE_UNAVAILABLE_NOTE: &str = "[image omitted: the current model cannot see images from tools; do not try to get the image another way; tell the user if you need it]";

/// Appended to a tool message's content when an older image was dropped to
/// stay within the per-request image budget.
pub const SUPERSEDED_TOOL_IMAGE_NOTE: &str = "[image omitted: superseded by a newer one]";

/// The most tool images a single request carries: only the newest this many
/// are sent, and every older one is replaced with `SUPERSEDED_TOOL_IMAGE_NOTE`.
/// Compaction snapshots keep the same newest images, since a superseded image
/// is never sent again; each tool result's own history record keeps all of its
/// images.
pub const MAX_REQUEST_TOOL_IMAGES: usize = 3;

/// Why a candidate tool image could not become an `ImageInput`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolImageRejected {
    UnsupportedType(String),
    TooLarge { bytes: usize },
    InvalidBase64,
    NotAnImage,
}

impl ToolImageRejected {
    /// A model-facing note explaining why the image was omitted, in the same
    /// style as other tool output notices.
    pub fn note(&self) -> String {
        match self {
            Self::UnsupportedType(media_type) if media_type.is_empty() => {
                "[image omitted: missing media type]".to_string()
            }
            Self::UnsupportedType(media_type) => {
                format!("[image omitted: unsupported type {media_type}]")
            }
            Self::TooLarge { bytes } => {
                let mib = *bytes as f64 / (1024.0 * 1024.0);
                let limit_mib = MAX_TOOL_IMAGE_BYTES / (1024 * 1024);
                format!("[image omitted: {mib:.1} MiB is over the {limit_mib} MiB limit]")
            }
            Self::InvalidBase64 => "[image omitted: invalid base64 data]".to_string(),
            Self::NotAnImage => {
                "[image omitted: the data is not a PNG, JPEG, GIF or WebP image]".to_string()
            }
        }
    }
}

/// Detects supported image types from their file headers.
pub fn sniff_image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

/// Validates a tool-supplied image and wraps it as an `ImageInput`.
///
/// Rejects invalid or empty base64 and decoded payloads larger than
/// `MAX_TOOL_IMAGE_BYTES`. The file header determines the type regardless of
/// the declaration. Unrecognized bytes produce an unsupported-type note if
/// the declaration is unsupported, or a not-an-image note otherwise.
pub fn tool_image(media_type: &str, base64_data: String) -> Result<ImageInput, ToolImageRejected> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&base64_data)
        .map_err(|_| ToolImageRejected::InvalidBase64)?;
    if decoded.is_empty() {
        return Err(ToolImageRejected::InvalidBase64);
    }
    if decoded.len() > MAX_TOOL_IMAGE_BYTES {
        return Err(ToolImageRejected::TooLarge {
            bytes: decoded.len(),
        });
    }
    let media_type = sniff_image_type(&decoded).ok_or_else(|| {
        if TOOL_IMAGE_MEDIA_TYPES.contains(&media_type) {
            ToolImageRejected::NotAnImage
        } else {
            ToolImageRejected::UnsupportedType(media_type.to_string())
        }
    })?;
    Ok(ImageInput {
        source: ImageSource::Base64 {
            media_type: media_type.to_string(),
            data: base64_data,
        },
        detail: ImageDetail::High,
    })
}

/// Clears every tool message's images and appends `note` once to each tool
/// message that had any. User message images are untouched. Returns the
/// number of images dropped.
///
/// Callers use it wherever tool images must go no further. Tool images are
/// stored as assets with each tool result, but a resumed session never reloads
/// them (`RESUMED_TOOL_IMAGE_NOTE`), and a request to a model that cannot take
/// images in tool messages carries a note instead, on both the main and the
/// child-agent turn path (`TOOL_IMAGE_UNAVAILABLE_NOTE`).
pub fn drop_tool_images(messages: &mut [Message], note: &str) -> usize {
    let mut dropped = 0usize;
    for message in messages.iter_mut() {
        let Message::Tool {
            content, images, ..
        } = message
        else {
            continue;
        };
        if images.is_empty() {
            continue;
        }
        dropped += images.len();
        images.clear();
        content.push('\n');
        content.push_str(note);
    }
    dropped
}

/// Keeps only the newest `keep` tool images across `messages`, ordered by
/// message position and then by each message's own image order; every older
/// image is dropped and its message gets `SUPERSEDED_TOOL_IMAGE_NOTE`
/// appended once. Returns the number of images dropped.
///
/// Used on live conversations, requests and compaction snapshots to bound
/// retained tool images. Each tool result's own history record keeps all of
/// its images for on-demand reads from disk.
pub fn keep_newest_tool_images(messages: &mut [Message], keep: usize) -> usize {
    let total: usize = messages
        .iter()
        .map(|message| match message {
            Message::Tool { images, .. } => images.len(),
            _ => 0,
        })
        .sum();
    let mut to_drop = total.saturating_sub(keep);
    let mut dropped = 0usize;
    for message in messages.iter_mut() {
        if to_drop == 0 {
            break;
        }
        let Message::Tool {
            content, images, ..
        } = message
        else {
            continue;
        };
        if images.is_empty() {
            continue;
        }
        let remove = images.len().min(to_drop);
        images.drain(0..remove);
        to_drop -= remove;
        dropped += remove;
        content.push('\n');
        content.push_str(SUPERSEDED_TOOL_IMAGE_NOTE);
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny, valid 1x1 PNG (base64-encoded). Later tasks that need a
    /// sample image should define their own constant the same way rather
    /// than reuse this one across modules.
    const BASE64_1X1_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    #[test]
    fn a_mislabelled_png_is_kept_as_png() {
        let image = tool_image("image/jpeg", BASE64_1X1_PNG.to_string()).unwrap();
        assert!(
            matches!(image.source, ImageSource::Base64 { ref media_type, .. } if media_type == "image/png")
        );
    }

    #[test]
    fn bytes_that_are_no_image_are_dropped_with_a_note() {
        let rejected = tool_image("image/png", "aGVsbG8=".to_string()).unwrap_err();
        assert_eq!(
            rejected.note(),
            "[image omitted: the data is not a PNG, JPEG, GIF or WebP image]"
        );
    }

    #[test]
    fn a_mislabelled_image_uses_its_signature_even_for_unsupported_declarations() {
        for (bytes, expected) in [
            (b"\x89PNG\r\n\x1a\n".as_slice(), "image/png"),
            (b"\xff\xd8\xff".as_slice(), "image/jpeg"),
            (b"GIF87a".as_slice(), "image/gif"),
            (b"GIF89a".as_slice(), "image/gif"),
            (b"RIFF\x00\x00\x00\x00WEBP".as_slice(), "image/webp"),
        ] {
            for declared in ["image/png", "image/svg+xml", ""] {
                let image = tool_image(
                    declared,
                    base64::engine::general_purpose::STANDARD.encode(bytes),
                )
                .unwrap();
                assert!(
                    matches!(image.source, ImageSource::Base64 { ref media_type, .. } if media_type == expected)
                );
            }
        }
    }

    #[test]
    fn bytes_that_are_no_image_include_incomplete_or_wrong_signatures() {
        for bytes in [
            b"\x89PNG\r\n\x1a".as_slice(),
            b"\xff\xd8".as_slice(),
            b"GIF88a".as_slice(),
            b"RIFF0000WEB".as_slice(),
            b"RIFF0000WAVE".as_slice(),
            b"NOPE0000WEBP".as_slice(),
        ] {
            let rejected = tool_image(
                "image/png",
                base64::engine::general_purpose::STANDARD.encode(bytes),
            )
            .unwrap_err();
            assert_eq!(
                rejected.note(),
                "[image omitted: the data is not a PNG, JPEG, GIF or WebP image]"
            );
        }
    }

    #[test]
    fn tool_image_accepts_supported_types_within_the_limit() {
        let image = tool_image("image/png", BASE64_1X1_PNG.to_string()).unwrap();
        assert!(
            matches!(image.source, ImageSource::Base64 { ref media_type, .. } if media_type == "image/png")
        );
        assert_eq!(image.detail, ImageDetail::High);
    }

    #[test]
    fn tool_image_rejects_an_unsupported_type_with_a_note() {
        let rejected = tool_image("image/svg+xml", "PHN2Zz4=".to_string()).unwrap_err();
        assert_eq!(
            rejected.note(),
            "[image omitted: unsupported type image/svg+xml]"
        );
    }

    #[test]
    fn tool_image_rejects_invalid_base64_with_a_note() {
        let rejected = tool_image("image/png", "not base64!".to_string()).unwrap_err();
        assert_eq!(rejected.note(), "[image omitted: invalid base64 data]");
    }

    #[test]
    fn tool_image_rejects_data_that_decodes_to_nothing() {
        assert_eq!(
            tool_image("image/png", String::new()),
            Err(ToolImageRejected::InvalidBase64)
        );
    }

    #[test]
    fn tool_image_rejects_an_image_over_5_mib() {
        let data =
            base64::engine::general_purpose::STANDARD.encode(vec![0u8; MAX_TOOL_IMAGE_BYTES + 1]);
        assert!(matches!(
            tool_image("image/png", data),
            Err(ToolImageRejected::TooLarge { .. })
        ));
    }

    fn sample_image(label: &str) -> ImageInput {
        ImageInput {
            source: ImageSource::Base64 {
                media_type: "image/png".to_string(),
                data: match label {
                    "a" => "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNg1LD5DwAB+AFlawEwEQAAAABJRU5ErkJggg==",
                    "b" => "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNg0rD5DwAB/AFmhhuw/QAAAABJRU5ErkJggg==",
                    "c" => "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNg1rD5DwACAAFnApv0PQAAAABJRU5ErkJggg==",
                    "d" => "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNg0bD5DwACBAFo8UwsuQAAAABJRU5ErkJggg==",
                    "e" => "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNg1bD5DwACCAFp0r20RwAAAABJRU5ErkJggg==",
                    "user" => "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNg07D5DwACDAFqP6c0qwAAAABJRU5ErkJggg==",
                    "tool-a" => "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNg17D5DwACEAFrAHAPJQAAAABJRU5ErkJggg==",
                    "tool-b" => "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGPg0LD5DwACFAFsG3lWkgAAAABJRU5ErkJggg==",
                    _ => panic!("unknown test image label: {label}"),
                }.to_string(),
            },
            detail: ImageDetail::High,
        }
    }

    #[test]
    fn drop_tool_images_leaves_a_note_and_keeps_user_images() {
        let mut messages = vec![
            Message::user_with_images("look at this".to_string(), vec![sample_image("user")]),
            Message::Tool {
                tool_call_id: "call_1".to_string(),
                content: "screenshot taken".to_string(),
                images: vec![sample_image("tool-a"), sample_image("tool-b")],
                terminal: None,
                pinned: false,
            },
            Message::Tool {
                tool_call_id: "call_2".to_string(),
                content: "no screenshot".to_string(),
                images: Vec::new(),
                terminal: None,
                pinned: false,
            },
        ];

        let dropped = drop_tool_images(&mut messages, "N");

        assert_eq!(dropped, 2);
        assert_eq!(messages[0].images(), &[sample_image("user")]);
        match &messages[1] {
            Message::Tool {
                images, content, ..
            } => {
                assert!(images.is_empty());
                assert_eq!(content, "screenshot taken\nN");
            }
            other => panic!("expected Tool message, got {other:?}"),
        }
        match &messages[2] {
            Message::Tool { content, .. } => assert_eq!(content, "no screenshot"),
            other => panic!("expected Tool message, got {other:?}"),
        }
    }

    #[test]
    fn keep_newest_tool_images_drops_the_oldest_first() {
        let mut messages = vec![
            Message::Tool {
                tool_call_id: "call_1".to_string(),
                content: "first".to_string(),
                images: vec![sample_image("a"), sample_image("b")],
                terminal: None,
                pinned: false,
            },
            Message::Tool {
                tool_call_id: "call_2".to_string(),
                content: "second".to_string(),
                images: vec![sample_image("c")],
                terminal: None,
                pinned: false,
            },
            Message::Tool {
                tool_call_id: "call_3".to_string(),
                content: "third".to_string(),
                images: vec![sample_image("d"), sample_image("e")],
                terminal: None,
                pinned: false,
            },
        ];

        let dropped = keep_newest_tool_images(&mut messages, 3);

        assert_eq!(dropped, 2);
        match &messages[0] {
            Message::Tool {
                images, content, ..
            } => {
                assert!(images.is_empty());
                assert_eq!(content, &format!("first\n{SUPERSEDED_TOOL_IMAGE_NOTE}"));
            }
            other => panic!("expected Tool message, got {other:?}"),
        }
        match &messages[1] {
            Message::Tool {
                images, content, ..
            } => {
                assert_eq!(images, &[sample_image("c")]);
                assert_eq!(content, "second");
            }
            other => panic!("expected Tool message, got {other:?}"),
        }
        match &messages[2] {
            Message::Tool {
                images, content, ..
            } => {
                assert_eq!(images, &[sample_image("d"), sample_image("e")]);
                assert_eq!(content, "third");
            }
            other => panic!("expected Tool message, got {other:?}"),
        }
    }

    #[test]
    fn keep_newest_tool_images_drops_a_partly_kept_messages_oldest_images_first() {
        let mut messages = vec![
            Message::Tool {
                tool_call_id: "call_1".to_string(),
                content: "first".to_string(),
                images: vec![sample_image("a"), sample_image("b"), sample_image("c")],
                terminal: None,
                pinned: false,
            },
            Message::Tool {
                tool_call_id: "call_2".to_string(),
                content: "second".to_string(),
                images: vec![sample_image("d")],
                terminal: None,
                pinned: false,
            },
        ];

        let dropped = keep_newest_tool_images(&mut messages, 3);

        assert_eq!(dropped, 1);
        match &messages[0] {
            Message::Tool {
                images, content, ..
            } => {
                assert_eq!(images, &[sample_image("b"), sample_image("c")]);
                assert_eq!(content, &format!("first\n{SUPERSEDED_TOOL_IMAGE_NOTE}"));
            }
            other => panic!("expected Tool message, got {other:?}"),
        }
        match &messages[1] {
            Message::Tool {
                images, content, ..
            } => {
                assert_eq!(images, &[sample_image("d")]);
                assert_eq!(content, "second");
            }
            other => panic!("expected Tool message, got {other:?}"),
        }
    }
}
