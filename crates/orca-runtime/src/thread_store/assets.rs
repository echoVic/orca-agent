//! Versioned disk-only image indirection. Live surface digests and provider
//! inputs keep their original representation; hydration precedes their validation.
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::writer::{MAX_SESSION_LINE_BYTES, open_regular_history_file};

const RECORD_TYPE: &str = "session.record_with_assets";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AssetRecord {
    #[serde(rename = "type")]
    record_type: String,
    version: u32,
    record: Value,
    assets: Vec<ImageAsset>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ImageAsset {
    /// JSON pointer to the original base64 string, not a filesystem path.
    pointer: String,
    sha256: String,
    bytes: u64,
}

pub(crate) fn directory(path: &Path) -> PathBuf {
    let plain = if path.extension().and_then(|s| s.to_str()) == Some("zst") {
        path.with_extension("")
    } else {
        path.to_path_buf()
    };
    let mut name = plain.as_os_str().to_os_string();
    name.push(".assets");
    PathBuf::from(name)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn validate_digest(digest: &str) -> io::Result<()> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid("invalid image asset digest"));
    }
    Ok(())
}

fn check_directory(root: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.file_type().is_dir() {
        return Err(invalid("image asset directory must not be a symlink"));
    }
    Ok(())
}

fn read_blob(root: &Path, asset: &ImageAsset) -> io::Result<Vec<u8>> {
    validate_digest(&asset.sha256)?;
    check_directory(root)?;
    if asset.bytes > MAX_SESSION_LINE_BYTES as u64 {
        return Err(invalid("image asset exceeds record budget"));
    }
    let file = open_regular_history_file(&root.join(&asset.sha256))?;
    if file.metadata()?.len() != asset.bytes {
        return Err(invalid("image asset length mismatch"));
    }
    let mut bytes = Vec::new();
    file.take(asset.bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != asset.bytes || format!("{:x}", Sha256::digest(&bytes)) != asset.sha256
    {
        return Err(invalid("image asset digest mismatch"));
    }
    Ok(bytes)
}

fn put_blob(root: &Path, asset: &ImageAsset, bytes: &[u8]) -> io::Result<()> {
    fs::create_dir_all(root)?;
    check_directory(root)?;
    let target = root.join(&asset.sha256);
    if fs::symlink_metadata(&target).is_ok() {
        read_blob(root, asset)?;
        return Ok(());
    }
    // No-replace publication also supports nested transcript rewrites on
    // Windows, whose general atomic replacement helper holds a process lock.
    let mut temporary = tempfile::NamedTempFile::new_in(root)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(&target) {
        Ok(_) => {}
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            read_blob(root, asset)?;
        }
        Err(error) => return Err(error.error),
    }
    #[cfg(unix)]
    {
        fs::File::open(root)?.sync_all()?;
        if let Some(parent) = root.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
    }
    Ok(())
}

fn escape_pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Find both serialized image-source types (core's internally tagged source
/// and surface's externally tagged source). Strings, URLs and arbitrary data
/// fields are never interpreted as paths or fetched over the network.
fn image_data_pointers(value: &Value, path: &str, output: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("base64")
                && map.get("media_type").is_some_and(Value::is_string)
                && map.get("data").is_some_and(Value::is_string)
            {
                output.push(format!("{path}/data"));
                return;
            }
            if let Some(source) = map.get("Base64").and_then(Value::as_object)
                && source.get("media_type").is_some_and(Value::is_string)
                && source.get("data").is_some_and(Value::is_string)
                && source.contains_key("digest")
            {
                output.push(format!("{path}/Base64/data"));
                return;
            }
            for (key, child) in map {
                image_data_pointers(child, &format!("{path}/{}", escape_pointer(key)), output);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                image_data_pointers(child, &format!("{path}/{index}"), output);
            }
        }
        _ => {}
    }
}

pub(crate) fn externalize(path: &Path, mut record: Value) -> io::Result<Value> {
    let mut pointers = Vec::new();
    image_data_pointers(&record, "", &mut pointers);
    if pointers.is_empty() {
        return Ok(record);
    }
    let mut assets = Vec::new();
    for pointer in pointers {
        let value = record.pointer_mut(&pointer).expect("discovered image data");
        let data = value.as_str().expect("image data is a string");
        if data.len() > MAX_SESSION_LINE_BYTES {
            return Err(invalid("image data exceeds record budget"));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| invalid("invalid base64 image data"))?;
        // Preserve the exact bytes covered by surface batch digests.
        if base64::engine::general_purpose::STANDARD.encode(&bytes) != data {
            return Err(invalid("non-canonical base64 image data"));
        }
        let asset = ImageAsset {
            pointer,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            bytes: bytes.len() as u64,
        };
        put_blob(&directory(path), &asset, &bytes)?;
        *value = Value::String(String::new());
        assets.push(asset);
    }
    serde_json::to_value(AssetRecord {
        record_type: RECORD_TYPE.to_string(),
        version: 1,
        record,
        assets,
    })
    .map_err(io::Error::other)
}

pub(crate) fn is_asset_record(value: &Value) -> bool {
    value["type"].as_str() == Some(RECORD_TYPE)
}

/// Bounded index projections deliberately do not load image bytes.
pub(crate) fn hydrate(path: &Path, value: Value, load_images: bool) -> io::Result<Value> {
    if !is_asset_record(&value) {
        return Ok(value);
    }
    let mut envelope: AssetRecord = serde_json::from_value(value).map_err(io::Error::other)?;
    if envelope.version != 1 || is_asset_record(&envelope.record) {
        return Err(invalid("unsupported image asset record version"));
    }
    let mut expected = Vec::new();
    image_data_pointers(&envelope.record, "", &mut expected);
    let expected: BTreeSet<_> = expected.into_iter().collect();
    let mut seen = BTreeSet::new();
    let mut expanded_bytes = serde_json::to_vec(&envelope.record)
        .map_err(io::Error::other)?
        .len() as u64;
    for asset in envelope.assets {
        validate_digest(&asset.sha256)?;
        if !expected.contains(&asset.pointer) || !seen.insert(asset.pointer.clone()) {
            return Err(invalid("invalid or duplicate image asset pointer"));
        }
        expanded_bytes = expanded_bytes.saturating_add(asset.bytes.div_ceil(3).saturating_mul(4));
        if expanded_bytes >= MAX_SESSION_LINE_BYTES as u64 {
            return Err(invalid("expanded image record exceeds record budget"));
        }
        let data = envelope
            .record
            .pointer_mut(&asset.pointer)
            .ok_or_else(|| invalid("missing image asset pointer"))?;
        if data.as_str() != Some("") {
            return Err(invalid("image asset slot must be empty"));
        }
        if load_images {
            let bytes = read_blob(&directory(path), &asset)?;
            *data = Value::String(base64::engine::general_purpose::STANDARD.encode(bytes));
        }
    }
    if seen != expected || seen.is_empty() {
        return Err(invalid("incomplete image asset manifest"));
    }
    Ok(envelope.record)
}

/// Copy before publishing an archived transcript. A crash may leave an
/// unreferenced copy, but never a published transcript with missing assets.
pub(crate) fn copy_directory(from: &Path, to: &Path) -> io::Result<()> {
    let source = directory(from);
    if !source.exists() {
        return Ok(());
    }
    check_directory(&source)?;
    for entry in fs::read_dir(&source)? {
        let entry = entry?;
        let digest = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid("invalid asset filename"))?;
        validate_digest(&digest)?;
        let asset = ImageAsset {
            pointer: String::new(),
            sha256: digest,
            bytes: entry.metadata()?.len(),
        };
        let bytes = read_blob(&source, &asset)?;
        put_blob(&directory(to), &asset, &bytes)?;
    }
    Ok(())
}

pub(crate) fn remove_directory(path: &Path) -> io::Result<()> {
    let root = directory(path);
    match fs::symlink_metadata(&root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(_) => {
            check_directory(&root)?;
            fs::remove_dir_all(root)
        }
    }
}
