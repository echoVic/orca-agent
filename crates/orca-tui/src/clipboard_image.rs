use std::fmt;
use std::io::Cursor;
use std::path::{Path, PathBuf};

#[cfg(any(test, target_os = "linux"))]
use base64::Engine as _;
#[cfg(any(test, target_os = "linux", target_os = "windows"))]
use image::ImageEncoder as _;
#[cfg(any(test, target_os = "linux"))]
use serde::Deserialize;

pub(crate) const MAX_COMPOSER_IMAGE_BYTES: usize = 5 * 1024 * 1024;
pub(crate) const MAX_COMPOSER_IMAGE_COUNT: usize = 600;
pub(crate) const MAX_COMPOSER_IMAGE_PIXELS: u64 = 32_000_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImagePasteRequest {
    Clipboard,
    Paths(Vec<PathBuf>),
}

#[derive(Clone, Eq, PartialEq)]
pub struct ClipboardImagePayload {
    pub(crate) media_type: String,
    pub(crate) data: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) source_name: Option<String>,
}

impl fmt::Debug for ClipboardImagePayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClipboardImagePayload")
            .field("media_type", &self.media_type)
            .field("bytes", &self.data.len())
            .field("width", &self.width)
            .field("height", &self.height)
            .field("source_name", &self.source_name)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ClipboardImageError {
    RemoteClipboardUnavailable,
    ClipboardUnavailable(String),
    NoImage(String),
    UnsupportedImage(String),
    InvalidImage(String),
    LimitExceeded(String),
}

impl fmt::Display for ClipboardImageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RemoteClipboardUnavailable => formatter.write_str(
                "clipboard image paste is unavailable in this remote session; paste or @mention a remote image path",
            ),
            Self::ClipboardUnavailable(error) => {
                write!(formatter, "clipboard is unavailable: {error}")
            }
            Self::NoImage(error) => write!(formatter, "clipboard does not contain an image: {error}"),
            Self::UnsupportedImage(error) => write!(formatter, "unsupported image: {error}"),
            Self::InvalidImage(error) => write!(formatter, "invalid image: {error}"),
            Self::LimitExceeded(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for ClipboardImageError {}

pub(crate) fn read_image_request(
    request: ImagePasteRequest,
) -> Result<Vec<ClipboardImagePayload>, ClipboardImageError> {
    match request {
        ImagePasteRequest::Clipboard => read_clipboard_images(),
        ImagePasteRequest::Paths(paths) => read_image_paths(&paths),
    }
}

pub(crate) fn image_paths_from_paste(pasted: &str) -> Option<Vec<PathBuf>> {
    const MAX_PATH_PASTE_BYTES: usize = 64 * 1024;

    let pasted = pasted.trim();
    if pasted.is_empty() || pasted.len() > MAX_PATH_PASTE_BYTES {
        return None;
    }
    if let Some(path) = path_from_paste_token(strip_matching_quotes(pasted))
        && path.is_file()
        && has_supported_image_extension(&path)
    {
        return Some(vec![path]);
    }
    let tokens = pasted_path_tokens(pasted)?;
    if tokens.is_empty() {
        return None;
    }
    let mut paths = Vec::with_capacity(tokens.len());
    for token in tokens {
        let path = path_from_paste_token(&token)?;
        if !path.is_file() || !has_supported_image_extension(&path) {
            return None;
        }
        paths.push(path);
    }
    Some(paths)
}

fn strip_matching_quotes(value: &str) -> &str {
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn path_from_paste_token(token: &str) -> Option<PathBuf> {
    if token.starts_with("file://") {
        url::Url::parse(token).ok()?.to_file_path().ok()
    } else {
        Some(PathBuf::from(token))
    }
}

#[cfg(not(windows))]
fn pasted_path_tokens(pasted: &str) -> Option<Vec<String>> {
    shlex::split(pasted)
}

#[cfg(windows)]
fn pasted_path_tokens(pasted: &str) -> Option<Vec<String>> {
    use std::os::windows::ffi::OsStrExt as _;

    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

    let command_line = std::ffi::OsStr::new(pasted)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut argument_count = 0;
    let arguments = unsafe { CommandLineToArgvW(command_line.as_ptr(), &mut argument_count) };
    if arguments.is_null() || argument_count <= 0 {
        return None;
    }
    let values = (0..argument_count)
        .map(|index| {
            let argument = unsafe { *arguments.add(index as usize) };
            let length = (0..)
                .take_while(|offset| unsafe { *argument.add(*offset) } != 0)
                .count();
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(argument, length) })
        })
        .collect();
    unsafe {
        LocalFree(arguments.cast());
    }
    Some(values)
}

fn read_clipboard_images() -> Result<Vec<ClipboardImagePayload>, ClipboardImageError> {
    if remote_without_graphical_clipboard() {
        return Err(ClipboardImageError::RemoteClipboardUnavailable);
    }
    read_platform_clipboard_images()
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn read_arboard_clipboard_images() -> Result<Vec<ClipboardImagePayload>, ClipboardImageError> {
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|error| ClipboardImageError::ClipboardUnavailable(error.to_string()))?;
    let files = clipboard
        .get()
        .file_list()
        .map_err(|error| ClipboardImageError::ClipboardUnavailable(error.to_string()))
        .unwrap_or_default();
    let image_paths = files
        .into_iter()
        .filter(|path| path.is_file() && has_supported_image_extension(path))
        .collect::<Vec<_>>();
    if !image_paths.is_empty() {
        return read_image_paths(&image_paths);
    }

    let image = clipboard
        .get_image()
        .map_err(|error| ClipboardImageError::NoImage(error.to_string()))?;
    let width = u32::try_from(image.width)
        .map_err(|_| ClipboardImageError::InvalidImage("width is too large".to_string()))?;
    let height = u32::try_from(image.height)
        .map_err(|_| ClipboardImageError::InvalidImage("height is too large".to_string()))?;
    if width == 0 || height == 0 {
        return Err(ClipboardImageError::InvalidImage(
            "dimensions must be non-zero".to_string(),
        ));
    }
    enforce_dimension_limit(width, height)?;
    let expected = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| ClipboardImageError::InvalidImage("RGBA dimensions overflow".to_string()))?;
    if image.bytes.len() != expected {
        return Err(ClipboardImageError::InvalidImage(format!(
            "RGBA payload has {} bytes; expected {expected}",
            image.bytes.len()
        )));
    }

    let mut encoded = Vec::new();
    image::codecs::png::PngEncoder::new(&mut Cursor::new(&mut encoded))
        .write_image(
            image.bytes.as_ref(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|error| ClipboardImageError::InvalidImage(error.to_string()))?;
    enforce_encoded_limit(encoded.len())?;
    Ok(vec![ClipboardImagePayload {
        media_type: "image/png".to_string(),
        data: encoded,
        width,
        height,
        source_name: None,
    }])
}

#[cfg(target_os = "windows")]
fn read_platform_clipboard_images() -> Result<Vec<ClipboardImagePayload>, ClipboardImageError> {
    read_arboard_clipboard_images()
}

#[cfg(target_os = "linux")]
fn read_platform_clipboard_images() -> Result<Vec<ClipboardImagePayload>, ClipboardImageError> {
    if is_wsl() {
        match read_wsl_clipboard_images() {
            Ok(images) => return Ok(images),
            Err(ClipboardImageError::ClipboardUnavailable(wsl_error)) => {
                return read_arboard_clipboard_images().map_err(|arboard_error| {
                    ClipboardImageError::ClipboardUnavailable(format!(
                        "Windows clipboard helper failed ({wsl_error}); Linux clipboard fallback failed ({arboard_error})"
                    ))
                });
            }
            Err(error) => return Err(error),
        }
    }
    read_arboard_clipboard_images()
}

#[cfg(target_os = "macos")]
fn read_platform_clipboard_images() -> Result<Vec<ClipboardImagePayload>, ClipboardImageError> {
    let image_paths = macos_clipboard_file_paths()
        .into_iter()
        .filter(|path| path.is_file() && has_supported_image_extension(path))
        .collect::<Vec<_>>();
    if !image_paths.is_empty() {
        return read_image_paths(&image_paths);
    }

    let path = std::env::temp_dir().join(format!(
        "orca-clipboard-{}.png",
        uuid::Uuid::new_v4().simple()
    ));
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| ClipboardImageError::ClipboardUnavailable(error.to_string()))?;
    }
    let script = r#"
on run argv
  set outputPath to item 1 of argv
  set imageData to the clipboard as «class PNGf»
  set fileRef to open for access POSIX file outputPath with write permission
  try
    set eof fileRef to 0
    write imageData to fileRef
    close access fileRef
  on error messageText
    try
      close access fileRef
    end try
    error messageText
  end try
end run
"#;
    let mut command = std::process::Command::new("osascript");
    command
        .args(["-e", script, "--"])
        .arg(&path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let cwd = std::env::current_dir().map_err(|error| {
        ClipboardImageError::ClipboardUnavailable(format!("failed to resolve cwd: {error}"))
    })?;
    let output = match orca_tools::process::spawn_user_trusted(command, "tui:clipboard-image", &cwd)
        .and_then(|(child, process_job, _receipt)| {
            orca_tools::process::wait_for_child_output_with_timeout(
                child,
                process_job,
                std::time::Duration::from_secs(5),
            )
        }) {
        Ok(output) => output,
        Err(error) => {
            let _ = std::fs::remove_file(&path);
            return Err(ClipboardImageError::ClipboardUnavailable(error.to_string()));
        }
    };
    if !output.status.success() {
        let _ = std::fs::remove_file(&path);
        let message = output.stderr_text().trim().to_string();
        return Err(ClipboardImageError::NoImage(if message.is_empty() {
            "the macOS pasteboard has no PNG-compatible image".to_string()
        } else {
            message
        }));
    }
    let result = read_image_path(&path).map(|mut image| {
        image.source_name = None;
        image
    });
    let _ = std::fs::remove_file(&path);
    result.map(|image| vec![image])
}

#[cfg(target_os = "macos")]
fn macos_clipboard_file_paths() -> Vec<PathBuf> {
    let script = r#"
ObjC.import("AppKit");
const pasteboard = $.NSPasteboard.generalPasteboard;
const paths = [];
const seen = {};
function addPath(path) {
  if (path && !seen[path]) {
    seen[path] = true;
    paths.push(path);
  }
}
const legacyFiles = pasteboard.propertyListForType($.NSFilenamesPboardType);
if (legacyFiles) {
  for (let index = 0; index < Number(legacyFiles.count); index += 1) {
    addPath(legacyFiles.objectAtIndex(index).js);
  }
}
const items = pasteboard.pasteboardItems;
if (items) {
  for (let index = 0; index < Number(items.count); index += 1) {
    const value = items.objectAtIndex(index).stringForType($.NSPasteboardTypeFileURL);
    if (value) {
      const url = $.NSURL.URLWithString(value);
      if (url && url.isFileURL) {
        addPath(url.path.js);
      }
    }
  }
}
JSON.stringify(paths);
"#;
    let mut command = std::process::Command::new("osascript");
    command
        .args(["-l", "JavaScript", "-e", script])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };
    let Ok(output) = orca_tools::process::spawn_user_trusted(command, "tui:clipboard-paths", &cwd)
        .and_then(|(child, process_job, _receipt)| {
            orca_tools::process::wait_for_child_output_with_timeout(
                child,
                process_job,
                std::time::Duration::from_secs(5),
            )
        })
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    serde_json::from_slice::<Vec<String>>(&output.stdout)
        .unwrap_or_default()
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn read_platform_clipboard_images() -> Result<Vec<ClipboardImagePayload>, ClipboardImageError> {
    Err(ClipboardImageError::ClipboardUnavailable(
        "this platform has no clipboard image backend".to_string(),
    ))
}

fn read_image_paths(paths: &[PathBuf]) -> Result<Vec<ClipboardImagePayload>, ClipboardImageError> {
    if paths.len() > MAX_COMPOSER_IMAGE_COUNT {
        return Err(ClipboardImageError::LimitExceeded(format!(
            "image attachment count exceeds Orca's {MAX_COMPOSER_IMAGE_COUNT}-image limit"
        )));
    }
    let mut total = 0usize;
    let mut images = Vec::with_capacity(paths.len());
    for path in paths {
        let image = read_image_path(path)?;
        total = total.checked_add(image.data.len()).ok_or_else(|| {
            ClipboardImageError::LimitExceeded("image attachment size overflow".to_string())
        })?;
        enforce_encoded_limit(total)?;
        images.push(image);
    }
    Ok(images)
}

fn read_image_path(path: &Path) -> Result<ClipboardImagePayload, ClipboardImageError> {
    let data = std::fs::read(path).map_err(|error| {
        ClipboardImageError::InvalidImage(format!("failed to read {}: {error}", path.display()))
    })?;
    payload_from_bytes(
        data,
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned()),
        &path.display().to_string(),
    )
}

fn payload_from_bytes(
    data: Vec<u8>,
    source_name: Option<String>,
    source_label: &str,
) -> Result<ClipboardImagePayload, ClipboardImageError> {
    enforce_encoded_limit(data.len())?;
    let media_type = media_type_from_bytes(&data).ok_or_else(|| {
        ClipboardImageError::UnsupportedImage(format!(
            "{source_label} is not JPEG, PNG, GIF, or WebP"
        ))
    })?;
    let (width, height) = image::ImageReader::new(Cursor::new(&data))
        .with_guessed_format()
        .map_err(|error| {
            ClipboardImageError::InvalidImage(format!("failed to inspect {source_label}: {error}"))
        })?
        .into_dimensions()
        .map_err(|error| {
            ClipboardImageError::InvalidImage(format!("failed to decode {source_label}: {error}"))
        })?;
    enforce_dimension_limit(width, height)?;
    Ok(ClipboardImagePayload {
        media_type: media_type.to_string(),
        data,
        width,
        height,
        source_name,
    })
}

#[cfg(any(test, target_os = "linux"))]
#[derive(Deserialize)]
struct EncodedClipboardPayloads {
    images: Vec<EncodedClipboardPayload>,
}

#[cfg(any(test, target_os = "linux"))]
#[derive(Deserialize)]
struct EncodedClipboardPayload {
    data: String,
    source_name: Option<String>,
}

#[cfg(any(test, target_os = "linux"))]
fn decode_clipboard_payloads(
    encoded: &[u8],
) -> Result<Vec<ClipboardImagePayload>, ClipboardImageError> {
    let encoded: EncodedClipboardPayloads = serde_json::from_slice(encoded).map_err(|error| {
        ClipboardImageError::InvalidImage(format!(
            "clipboard helper returned invalid JSON: {error}"
        ))
    })?;
    if encoded.images.is_empty() {
        return Err(ClipboardImageError::NoImage(
            "the clipboard has no image data".to_string(),
        ));
    }
    if encoded.images.len() > MAX_COMPOSER_IMAGE_COUNT {
        return Err(ClipboardImageError::LimitExceeded(format!(
            "image attachment count exceeds Orca's {MAX_COMPOSER_IMAGE_COUNT}-image limit"
        )));
    }
    let mut total = 0usize;
    let mut images = Vec::with_capacity(encoded.images.len());
    for (index, encoded) in encoded.images.into_iter().enumerate() {
        let data = base64::engine::general_purpose::STANDARD
            .decode(encoded.data)
            .map_err(|error| {
                ClipboardImageError::InvalidImage(format!(
                    "clipboard helper returned invalid base64 for image {}: {error}",
                    index + 1
                ))
            })?;
        total = total.checked_add(data.len()).ok_or_else(|| {
            ClipboardImageError::LimitExceeded("image attachment size overflow".to_string())
        })?;
        enforce_encoded_limit(total)?;
        images.push(payload_from_bytes(
            data,
            encoded.source_name,
            "clipboard image",
        )?);
    }
    Ok(images)
}

#[cfg(target_os = "linux")]
fn is_wsl() -> bool {
    std::env::var_os("WSL_INTEROP").is_some()
        || std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"))
}

#[cfg(target_os = "linux")]
fn read_wsl_clipboard_images() -> Result<Vec<ClipboardImagePayload>, ClipboardImageError> {
    let script = format!(
        r#"
$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$maxBytes = {max_bytes}
$maxCount = {max_count}
$records = @()
$files = [System.Windows.Forms.Clipboard]::GetFileDropList()
$imageFiles = @($files | Where-Object {{ $_ -match '.(png|jpe?g|gif|webp)$' }})
if ($imageFiles.Count -gt 0) {{
  if ($imageFiles.Count -gt $maxCount) {{ throw "clipboard image count exceeds limit" }}
  $total = 0
  foreach ($file in $imageFiles) {{
    $bytes = [System.IO.File]::ReadAllBytes($file)
    $total += $bytes.Length
    if ($total -gt $maxBytes) {{ throw "clipboard image bytes exceed limit" }}
    $records += [pscustomobject]@{{
      data = [Convert]::ToBase64String($bytes)
      source_name = [System.IO.Path]::GetFileName($file)
    }}
  }}
}} else {{
  $image = [System.Windows.Forms.Clipboard]::GetImage()
  if ($null -eq $image) {{
    [Console]::Error.Write("the Windows clipboard has no image")
    exit 3
  }}
  $stream = [System.IO.MemoryStream]::new()
  try {{
    $image.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
    $bytes = $stream.ToArray()
  }} finally {{
    $stream.Dispose()
    $image.Dispose()
  }}
  if ($bytes.Length -gt $maxBytes) {{ throw "clipboard image bytes exceed limit" }}
  $records += [pscustomobject]@{{
    data = [Convert]::ToBase64String($bytes)
    source_name = $null
  }}
}}
[pscustomobject]@{{ images = @($records) }} | ConvertTo-Json -Compress -Depth 3
"#,
        max_bytes = MAX_COMPOSER_IMAGE_BYTES,
        max_count = MAX_COMPOSER_IMAGE_COUNT,
    );
    let output = std::process::Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Sta",
            "-Command",
        ])
        .arg(script)
        .output()
        .map_err(|error| ClipboardImageError::ClipboardUnavailable(error.to_string()))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return if output.status.code() == Some(3) {
            Err(ClipboardImageError::NoImage(message))
        } else if message.contains("exceeds limit") {
            Err(ClipboardImageError::LimitExceeded(message))
        } else {
            Err(ClipboardImageError::ClipboardUnavailable(
                if message.is_empty() {
                    "Windows clipboard helper failed".to_string()
                } else {
                    message
                },
            ))
        };
    }
    decode_clipboard_payloads(&output.stdout)
}

fn enforce_encoded_limit(bytes: usize) -> Result<(), ClipboardImageError> {
    if bytes > MAX_COMPOSER_IMAGE_BYTES {
        return Err(ClipboardImageError::LimitExceeded(format!(
            "attached images exceed Orca's {} MiB inline limit",
            MAX_COMPOSER_IMAGE_BYTES / (1024 * 1024)
        )));
    }
    Ok(())
}

fn enforce_dimension_limit(width: u32, height: u32) -> Result<(), ClipboardImageError> {
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if pixels > MAX_COMPOSER_IMAGE_PIXELS {
        return Err(ClipboardImageError::LimitExceeded(format!(
            "image dimensions exceed Orca's {MAX_COMPOSER_IMAGE_PIXELS}-pixel preview limit"
        )));
    }
    Ok(())
}

fn media_type_from_bytes(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn has_supported_image_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "jpg" | "jpeg" | "png" | "gif" | "webp"
            )
        })
}

fn remote_without_graphical_clipboard() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some()
        && std::env::var_os("DISPLAY").is_none()
        && std::env::var_os("WAYLAND_DISPLAY").is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes() -> Vec<u8> {
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut Cursor::new(&mut bytes))
            .write_image(&[0xff, 0, 0, 0xff], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        bytes
    }

    #[test]
    fn pasted_image_paths_support_shell_escaping_quotes_and_file_urls() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("one image.png");
        let second = root.path().join("two.jpg");
        std::fs::write(&first, b"\x89PNG\r\n\x1a\n").unwrap();
        std::fs::write(&second, b"\xff\xd8\xff").unwrap();

        #[cfg(not(windows))]
        let escaped = format!(
            "{} {}",
            first.display().to_string().replace(' ', "\\ "),
            shlex::try_quote(second.to_string_lossy().as_ref()).unwrap()
        );
        #[cfg(windows)]
        let escaped = format!("\"{}\" \"{}\"", first.display(), second.display());
        assert_eq!(
            image_paths_from_paste(&escaped),
            Some(vec![first.clone(), second.clone()])
        );
        assert_eq!(
            image_paths_from_paste(url::Url::from_file_path(&first).unwrap().as_str()),
            Some(vec![first])
        );
    }

    #[test]
    fn pasted_image_path_with_unescaped_spaces_is_one_attachment() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("dragged image.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n").unwrap();

        assert_eq!(
            image_paths_from_paste(path.to_string_lossy().as_ref()),
            Some(vec![path])
        );
    }

    #[test]
    fn ordinary_text_and_mixed_file_pastes_remain_text() {
        let root = tempfile::tempdir().unwrap();
        let image = root.path().join("image.png");
        let text = root.path().join("notes.txt");
        std::fs::write(&image, b"\x89PNG\r\n\x1a\n").unwrap();
        std::fs::write(&text, b"notes").unwrap();

        assert_eq!(image_paths_from_paste("hello world"), None);
        assert_eq!(
            image_paths_from_paste(&format!("{} {}", image.display(), text.display())),
            None
        );
    }

    #[test]
    fn oversized_image_path_list_reaches_the_explicit_count_error() {
        let root = tempfile::tempdir().unwrap();
        let image = root.path().join("image.png");
        std::fs::write(&image, png_bytes()).unwrap();
        #[cfg(not(windows))]
        let token = shlex::try_quote(image.to_string_lossy().as_ref())
            .unwrap()
            .into_owned();
        #[cfg(windows)]
        let token = format!("\"{}\"", image.display());
        let pasted = std::iter::repeat_n(token, MAX_COMPOSER_IMAGE_COUNT + 1)
            .collect::<Vec<_>>()
            .join(" ");

        let paths = image_paths_from_paste(&pasted).expect("recognized image paths");
        assert!(matches!(
            read_image_request(ImagePasteRequest::Paths(paths)),
            Err(ClipboardImageError::LimitExceeded(message))
                if message.contains("600-image limit")
        ));
    }

    #[test]
    fn image_magic_bytes_cover_every_provider_format() {
        assert_eq!(
            media_type_from_bytes(b"\x89PNG\r\n\x1a\n"),
            Some("image/png")
        );
        assert_eq!(media_type_from_bytes(b"\xff\xd8\xff"), Some("image/jpeg"));
        assert_eq!(media_type_from_bytes(b"GIF87a"), Some("image/gif"));
        assert_eq!(media_type_from_bytes(b"GIF89a"), Some("image/gif"));
        assert_eq!(media_type_from_bytes(b"RIFFxxxxWEBP"), Some("image/webp"));
        assert_eq!(media_type_from_bytes(b"BM"), None);
    }

    #[test]
    fn clipboard_payload_debug_redacts_image_bytes() {
        let payload = ClipboardImagePayload {
            media_type: "image/png".to_string(),
            data: b"secret-image-bytes".to_vec(),
            width: 2,
            height: 1,
            source_name: Some("image.png".to_string()),
        };
        let debug = format!("{payload:?}");

        assert!(debug.contains("bytes: 18"));
        assert!(!debug.contains("secret-image-bytes"));
    }

    #[test]
    fn preview_dimension_limit_rejects_decompression_bombs() {
        assert!(enforce_dimension_limit(4_000, 4_000).is_ok());
        assert!(matches!(
            enforce_dimension_limit(8_000, 8_000),
            Err(ClipboardImageError::LimitExceeded(message))
                if message.contains("32000000-pixel")
        ));
    }

    #[test]
    fn path_reader_validates_dimensions_mime_and_total_bytes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("pixel.png");
        std::fs::write(&path, png_bytes()).unwrap();

        let images = read_image_request(ImagePasteRequest::Paths(vec![path.clone()])).unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].media_type, "image/png");
        assert_eq!((images[0].width, images[0].height), (1, 1));
        assert_eq!(images[0].source_name.as_deref(), Some("pixel.png"));

        let invalid = root.path().join("invalid.png");
        std::fs::write(&invalid, b"not an image").unwrap();
        assert!(matches!(
            read_image_request(ImagePasteRequest::Paths(vec![invalid])),
            Err(ClipboardImageError::UnsupportedImage(_))
        ));
    }

    #[test]
    fn encoded_clipboard_payload_parser_preserves_multiple_files() {
        let first = base64::engine::general_purpose::STANDARD.encode(png_bytes());
        let second = base64::engine::general_purpose::STANDARD.encode(png_bytes());
        let encoded = serde_json::json!({
            "images": [
                {"data": first, "source_name": "一.png"},
                {"data": second, "source_name": "two.png"}
            ]
        });

        let images = decode_clipboard_payloads(encoded.to_string().as_bytes()).unwrap();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].source_name.as_deref(), Some("一.png"));
        assert_eq!(images[1].source_name.as_deref(), Some("two.png"));
        assert!(images.iter().all(|image| image.media_type == "image/png"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "reads the real macOS pasteboard"]
    fn real_macos_clipboard_image_smoke() {
        let images = read_image_request(ImagePasteRequest::Clipboard).unwrap();
        assert!(!images.is_empty());
        assert!(images.iter().all(|image| {
            matches!(
                image.media_type.as_str(),
                "image/jpeg" | "image/png" | "image/gif" | "image/webp"
            ) && image.width > 0
                && image.height > 0
        }));
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "replaces and reads the real macOS pasteboard"]
    fn real_macos_clipboard_file_list_preserves_all_images() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first image.png");
        let second = root.path().join("second image.png");
        std::fs::write(&first, png_bytes()).unwrap();
        std::fs::write(&second, png_bytes()).unwrap();
        let script = r#"
ObjC.import("AppKit");
function run(argv) {
  const files = $.NSMutableArray.alloc.init;
  argv.forEach(path => files.addObject($(path)));
  const pasteboard = $.NSPasteboard.generalPasteboard;
  pasteboard.clearContents;
  if (!pasteboard.setPropertyListForType(files, $.NSFilenamesPboardType)) {
    throw new Error("failed to write file paths to the pasteboard");
  }
}
"#;
        let output = std::process::Command::new("osascript")
            .args(["-l", "JavaScript", "-e", script, "--"])
            .arg(&first)
            .arg(&second)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let images = read_image_request(ImagePasteRequest::Clipboard).unwrap();

        assert_eq!(images.len(), 2);
        assert_eq!(images[0].source_name.as_deref(), Some("first image.png"));
        assert_eq!(images[1].source_name.as_deref(), Some("second image.png"));
    }
}
