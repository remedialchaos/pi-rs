//! Image clipboard helpers, ported from `utils/clipboard-image.ts`.
//!
//! Native clipboard APIs are outside this crate. Wayland/X11 helpers are driven through
//! `wl-paste`/`xclip`; image bytes are normalized to PNG with the `image` crate (upstream uses
//! Photon) and can be encoded as an OSC 52 base64 payload for terminal transfer.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use image::ImageFormat;

use super::clipboard::{ClipboardCommandSpec, ClipboardEnv, Platform, command};
use super::clipboard_command::{ClipboardCommandOpts, run_clipboard_command};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardImage {
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

pub const SUPPORTED_IMAGE_MIME_TYPES: [&str; 4] =
    ["image/png", "image/jpeg", "image/webp", "image/gif"];
pub const DEFAULT_LIST_TIMEOUT_MS: u64 = 1000;
pub const DEFAULT_POWERSHELL_TIMEOUT_MS: u64 = 5000;

pub fn base_mime_type(mime: &str) -> String {
    mime.split(';').next().unwrap_or(mime).trim().to_lowercase()
}

pub fn is_wayland_session(env: &ClipboardEnv) -> bool {
    env.wayland_display.is_some() || env.xdg_session_type.as_deref() == Some("wayland")
}

pub fn extension_for_image_mime_type(mime: &str) -> Option<&'static str> {
    match base_mime_type(mime).as_str() {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        _ => None,
    }
}

fn is_supported_image_mime_type(mime: &str) -> bool {
    let mime = base_mime_type(mime);
    SUPPORTED_IMAGE_MIME_TYPES
        .iter()
        .any(|supported| *supported == mime)
}

/// Prefer a known image type; otherwise accept the first `image/*` target.
pub fn select_preferred_image_mime_type(types: &[String]) -> Option<String> {
    let normalized: Vec<&str> = types
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect();
    for preferred in SUPPORTED_IMAGE_MIME_TYPES {
        if let Some(found) = normalized.iter().find(|value| **value == preferred) {
            return Some((*found).to_string());
        }
    }
    normalized
        .iter()
        .find(|value| value.starts_with("image/"))
        .map(|value| (*value).to_string())
}

fn list_types(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Commands used to write a PNG to the platform clipboard. Binary input is passed over stdin.
pub fn image_write_commands(platform: Platform, env: &ClipboardEnv) -> Vec<ClipboardCommandSpec> {
    match platform {
        Platform::Darwin => vec![command("pbcopy", &["-t", "image/png"])],
        Platform::Windows => vec![command("powershell.exe", &["-NoProfile", "-Command"])],
        _ => {
            let mut commands = Vec::new();
            if env.termux_version.is_none() {
                if is_wayland_session(env) {
                    commands.push(command("wl-copy", &["-t", "image/png"]));
                }
                if env.display.is_some() {
                    commands.push(command(
                        "xclip",
                        &["-selection", "clipboard", "-t", "image/png"],
                    ));
                }
            }
            commands
        }
    }
}

/// Re-encode bytes as PNG, preserving orientation. Unsupported formats are rejected.
pub fn to_png_bytes(bytes: &[u8], original: &[u8]) -> Result<Vec<u8>, String> {
    let image = image::load_from_memory(bytes).map_err(|error| error.to_string())?;
    let image = super::exif_orientation::apply_exif_orientation(image, original);
    let mut png = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    Ok(png)
}

/// OSC 52 payload for a PNG, for terminals that accept image clipboard writes.
pub fn encode_image_osc52(png: &[u8]) -> String {
    format!("\x1b]52;c;{}\x07", STANDARD.encode(png))
}

/// Copy image bytes to the clipboard. Any supported input format is converted to PNG first.
pub fn copy_image(
    bytes: &[u8],
    mime: &str,
    platform: Platform,
    env: &ClipboardEnv,
) -> Result<(), String> {
    let png = if is_supported_image_mime_type(mime) && base_mime_type(mime) == "image/png" {
        bytes.to_vec()
    } else {
        to_png_bytes(bytes, bytes)?
    };
    for item in image_write_commands(platform, env) {
        let opts = ClipboardCommandOpts {
            input: Some(png.clone()),
            timeout_ms: Some(5000),
            max_buffer_bytes: None,
        };
        if run_clipboard_command(item.program, &item.args, opts).is_some() {
            return Ok(());
        }
    }
    Err(super::clipboard::clipboard_error(platform, env, false))
}

fn read_via_wl_paste() -> Result<Option<ClipboardImage>, ()> {
    let list = run_clipboard_command(
        "wl-paste",
        &["--list-types".to_string()],
        ClipboardCommandOpts {
            timeout_ms: Some(DEFAULT_LIST_TIMEOUT_MS),
            ..ClipboardCommandOpts::default()
        },
    )
    .ok_or(())?;
    let Some(selected) = select_preferred_image_mime_type(&list_types(&list)) else {
        return Ok(None);
    };
    let data = run_clipboard_command(
        "wl-paste",
        &[
            "--type".to_string(),
            selected.clone(),
            "--no-newline".to_string(),
        ],
        ClipboardCommandOpts::default(),
    )
    .ok_or(())?;
    if data.is_empty() {
        return Ok(None);
    }
    Ok(Some(ClipboardImage {
        bytes: data,
        mime_type: base_mime_type(&selected).to_string(),
    }))
}

fn read_via_xclip() -> Result<Option<ClipboardImage>, ()> {
    let targets = run_clipboard_command(
        "xclip",
        &[
            "-selection".to_string(),
            "clipboard".to_string(),
            "-t".to_string(),
            "TARGETS".to_string(),
            "-o".to_string(),
        ],
        ClipboardCommandOpts {
            timeout_ms: Some(DEFAULT_LIST_TIMEOUT_MS),
            ..ClipboardCommandOpts::default()
        },
    )
    .ok_or(())?;
    let Some(preferred) = select_preferred_image_mime_type(&list_types(&targets)) else {
        return Ok(None);
    };
    let data = run_clipboard_command(
        "xclip",
        &[
            "-selection".to_string(),
            "clipboard".to_string(),
            "-t".to_string(),
            preferred.clone(),
            "-o".to_string(),
        ],
        ClipboardCommandOpts::default(),
    )
    .ok_or(())?;
    if data.is_empty() {
        return Ok(None);
    }
    Ok(Some(ClipboardImage {
        bytes: data,
        mime_type: base_mime_type(&preferred).to_string(),
    }))
}

/// Read image data from the clipboard. `None` means no image; `Err` reports a backend failure.
pub fn read_clipboard_image(
    platform: Platform,
    env: &ClipboardEnv,
) -> Result<Option<ClipboardImage>, String> {
    if env.termux_version.is_some() {
        return Ok(None);
    }
    let mut image = None;
    if platform == Platform::Linux {
        if is_wayland_session(env) {
            image = read_via_wl_paste().ok().flatten();
        }
        if image.is_none() {
            image = read_via_xclip().ok().flatten();
        }
    }
    let Some(image) = image else {
        return Ok(None);
    };
    if !is_supported_image_mime_type(&image.mime_type) {
        let png = to_png_bytes(&image.bytes, &image.bytes)?;
        return Ok(Some(ClipboardImage {
            bytes: png,
            mime_type: "image/png".to_string(),
        }));
    }
    Ok(Some(image))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::DynamicImage;

    #[test]
    fn extension_mapping() {
        assert_eq!(extension_for_image_mime_type("image/png"), Some("png"));
        assert_eq!(
            extension_for_image_mime_type("IMAGE/JPEG; charset=x"),
            Some("jpg")
        );
        assert_eq!(extension_for_image_mime_type("image/webp"), Some("webp"));
        assert_eq!(extension_for_image_mime_type("image/gif"), Some("gif"));
        assert_eq!(extension_for_image_mime_type("image/bmp"), None);
    }

    #[test]
    fn wayland_session_detection() {
        let mut env = ClipboardEnv::default();
        assert!(!is_wayland_session(&env));
        env.xdg_session_type = Some("wayland".to_string());
        assert!(is_wayland_session(&env));
        env.xdg_session_type = Some("x11".to_string());
        env.wayland_display = Some("wayland-0".to_string());
        assert!(is_wayland_session(&env));
    }

    #[test]
    fn preferred_mime_order() {
        let types = vec![
            "image/bmp".to_string(),
            "image/jpeg".to_string(),
            "image/png".to_string(),
            "UTF8_STRING".to_string(),
        ];
        assert_eq!(
            select_preferred_image_mime_type(&types).as_deref(),
            Some("image/png")
        );
        let only_bmp = vec!["image/bmp".to_string()];
        assert_eq!(
            select_preferred_image_mime_type(&only_bmp).as_deref(),
            Some("image/bmp")
        );
        assert_eq!(select_preferred_image_mime_type(&[]), None);
    }

    #[test]
    fn image_write_commands_follow_display() -> Result<(), String> {
        let wayland = image_write_commands(
            Platform::Linux,
            &ClipboardEnv {
                wayland_display: Some("wayland-0".to_string()),
                ..ClipboardEnv::default()
            },
        );
        assert_eq!(wayland[0].program, "wl-copy");
        assert_eq!(wayland[0].args, vec!["-t", "image/png"]);
        let x11 = image_write_commands(
            Platform::Linux,
            &ClipboardEnv {
                display: Some(":0".to_string()),
                ..ClipboardEnv::default()
            },
        );
        assert_eq!(x11[0].program, "xclip");
        assert_eq!(
            x11[0].args,
            vec!["-selection", "clipboard", "-t", "image/png"]
        );
        assert_eq!(
            image_write_commands(Platform::Darwin, &ClipboardEnv::default())[0].program,
            "pbcopy"
        );
        Ok(())
    }

    #[test]
    fn generated_png_is_converted_and_base64_encoded() -> Result<(), String> {
        let image = DynamicImage::new_rgba8(2, 2);
        let mut jpeg = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut jpeg), ImageFormat::Jpeg)
            .map_err(|e| e.to_string())?;
        let png = to_png_bytes(&jpeg, &jpeg)?;
        assert_eq!(&png[0..4], b"\x89PNG");
        let osc = encode_image_osc52(&png);
        assert!(osc.starts_with("\x1b]52;c;"));
        assert!(osc.ends_with('\x07'));
        let encoded = osc
            .trim_start_matches("\x1b]52;c;")
            .trim_end_matches('\x07');
        assert_eq!(STANDARD.decode(encoded).map_err(|e| e.to_string())?, png);
        Ok(())
    }

    #[test]
    fn termux_returns_no_image_without_running_backends() {
        let env = ClipboardEnv {
            termux_version: Some("1".to_string()),
            display: Some(":0".to_string()),
            ..ClipboardEnv::default()
        };
        assert_eq!(read_clipboard_image(Platform::Linux, &env), Ok(None));
    }
}
