//! Shared validation for a user-attached or agent-read image file.

use std::path::Path;

use base64::Engine;

/// Largest encoded image payload accepted by the chat and image tool.
pub const MAX_IMAGE_BASE64_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug)]
pub enum ImageLoadError {
    TooLarge,
    Unsupported,
    Unreadable(String),
}

pub struct LoadedImage {
    pub media_type: &'static str,
    pub data: String,
    pub byte_len: usize,
    pub file_name: String,
}

/// Identify image content from magic bytes, independent of the file extension.
pub fn image_media_type_from_magic(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub fn encoded_base64_len_within_ceiling(raw_len: u64) -> bool {
    match raw_len
        .checked_add(2)
        .map(|padded| padded / 3)
        .and_then(|groups| groups.checked_mul(4))
    {
        Some(encoded) => encoded <= MAX_IMAGE_BASE64_BYTES as u64,
        None => false,
    }
}

/// Read one existing image with a size preflight before allocating its payload.
pub fn load_image_file(path: &Path) -> Result<LoadedImage, ImageLoadError> {
    let metadata =
        std::fs::metadata(path).map_err(|error| ImageLoadError::Unreadable(error.to_string()))?;
    if !metadata.is_file() {
        return Err(ImageLoadError::Unreadable("not a regular file".to_string()));
    }
    if !encoded_base64_len_within_ceiling(metadata.len()) {
        return Err(ImageLoadError::TooLarge);
    }
    let bytes =
        std::fs::read(path).map_err(|error| ImageLoadError::Unreadable(error.to_string()))?;
    let media_type = image_media_type_from_magic(&bytes).ok_or(ImageLoadError::Unsupported)?;
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    if data.len() > MAX_IMAGE_BASE64_BYTES {
        return Err(ImageLoadError::TooLarge);
    }
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".to_string());
    Ok(LoadedImage {
        media_type,
        data,
        byte_len: bytes.len(),
        file_name,
    })
}
