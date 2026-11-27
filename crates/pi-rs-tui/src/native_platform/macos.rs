//! macOS clipboard and modifier keys, through the system's own tools and CoreGraphics.

use std::io::Write;
use std::process::{Command, Stdio};

use super::{ClipboardError, ClipboardRead, ModifierKey};

/// Writes the PNG data of the pasteboard to standard output, base64 encoded.
const IMAGE_SCRIPT: &str = r#"
ObjC.import('AppKit');
function out(text) {
  const data = $.NSString.stringWithString(text);
  $.NSFileHandle.fileHandleWithStandardOutput.writeData(data.dataUsingEncoding($.NSUTF8StringEncoding));
}
const pasteboard = $.NSPasteboard.generalPasteboard;
const data = pasteboard.dataForType($.NSPasteboardTypePNG);
if (data && data.length > 0) {
  out(data.base64EncodedStringWithOptions(0).js);
}
"#;

/// Writes the paths of the files on the pasteboard to standard output, one per line.
const FILE_PATHS_SCRIPT: &str = r#"
ObjC.import('AppKit');
function out(text) {
  const data = $.NSString.stringWithString(text);
  $.NSFileHandle.fileHandleWithStandardOutput.writeData(data.dataUsingEncoding($.NSUTF8StringEncoding));
}
const pasteboard = $.NSPasteboard.generalPasteboard;
const options = $.NSDictionary.dictionaryWithObjectForKey(true, $.NSPasteboardURLReadingFileURLsOnlyKey);
const urls = pasteboard.readObjectsForClassesOptions([$.NSURL], options);
if (urls) {
  for (let index = 0; index < urls.count; index++) {
    out(urls.objectAtIndex(index).path.js + "\n");
  }
}
"#;

/// The CoreGraphics modifier flags of the combined session state.
mod core_graphics {
    /// `kCGEventSourceStateCombinedSessionState`.
    const COMBINED_SESSION_STATE: u32 = 0;
    pub(super) const SHIFT: u64 = 0x0002_0000;
    pub(super) const CONTROL: u64 = 0x0004_0000;
    pub(super) const ALTERNATE: u64 = 0x0008_0000;
    pub(super) const COMMAND: u64 = 0x0010_0000;

    // SAFETY: `CGEventSourceFlagsState` is a CoreGraphics function that takes a session state id
    // and returns the modifier flags. It takes no pointers, cannot fail, and the flags are only
    // read.
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceFlagsState(state: u32) -> u64;
    }

    /// The modifier flags that are held down right now.
    pub(super) fn flags() -> u64 {
        // SAFETY: the call reads the session state and returns a plain integer.
        unsafe { CGEventSourceFlagsState(COMBINED_SESSION_STATE) }
    }
}

/// Runs a command and returns its standard output. `None` means the command is missing.
fn run(program: &str, args: &[&str], stdin: Option<&[u8]>) -> Option<Vec<u8>> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return None,
    };
    if let Some(bytes) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        let _ = pipe.write_all(bytes);
    }
    match child.wait_with_output() {
        Ok(output) if output.status.success() => Some(output.stdout),
        _ => None,
    }
}

/// The pasteboard text.
pub(super) fn get_text() -> Result<ClipboardRead<String>, ClipboardError> {
    let Some(bytes) = run("pbpaste", &[], None) else {
        return Err(ClipboardError);
    };
    if bytes.is_empty() {
        return Ok(ClipboardRead::Empty);
    }
    Ok(ClipboardRead::Data(
        String::from_utf8(bytes)
            .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()),
    ))
}

/// The pasteboard image, as PNG bytes.
pub(super) fn get_image() -> Result<ClipboardRead<Vec<u8>>, ClipboardError> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let Some(bytes) = run("osascript", &["-l", "JavaScript", "-e", IMAGE_SCRIPT], None) else {
        return Err(ClipboardError);
    };
    let encoded = String::from_utf8_lossy(&bytes);
    let encoded = encoded.trim();
    if encoded.is_empty() {
        return Ok(ClipboardRead::Empty);
    }
    let Ok(decoded) = STANDARD.decode(encoded) else {
        return Err(ClipboardError);
    };
    if decoded.is_empty() {
        return Ok(ClipboardRead::Empty);
    }
    Ok(ClipboardRead::Data(decoded))
}

/// The paths of the files on the pasteboard.
pub(super) fn get_file_paths() -> Result<ClipboardRead<Vec<String>>, ClipboardError> {
    let Some(bytes) = run(
        "osascript",
        &["-l", "JavaScript", "-e", FILE_PATHS_SCRIPT],
        None,
    ) else {
        return Err(ClipboardError);
    };
    let text = String::from_utf8_lossy(&bytes);
    let paths: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    if paths.is_empty() {
        return Ok(ClipboardRead::Empty);
    }
    Ok(ClipboardRead::Data(paths))
}

/// Writes text to the pasteboard.
pub(super) fn set_text(text: &str) -> Result<(), ClipboardError> {
    match run("pbcopy", &[], Some(text.as_bytes())) {
        Some(_) => Ok(()),
        None => Err(ClipboardError),
    }
}

/// Whether the modifier key is held down right now.
pub(super) fn is_modifier_pressed(modifier: ModifierKey) -> bool {
    let flags = core_graphics::flags();
    let mask = match modifier {
        ModifierKey::Shift => core_graphics::SHIFT,
        ModifierKey::Command => core_graphics::COMMAND,
        ModifierKey::Control => core_graphics::CONTROL,
        ModifierKey::Option => core_graphics::ALTERNATE,
    };
    flags & mask != 0
}
