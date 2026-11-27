//! Windows clipboard and modifier keys, through the system's own tools.

use std::io::Write;
use std::process::{Command, Stdio};

use super::{ClipboardError, ClipboardRead, ModifierKey};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL,
    VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
};

const KEY_PRESSED_MASK: i16 = -32768;

/// Runs a PowerShell script and returns its standard output. `None` means it failed.
fn powershell(script: &str) -> Option<Vec<u8>> {
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-STA", "-Command", script])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
}

/// The clipboard text.
pub(super) fn get_text() -> Result<ClipboardRead<String>, ClipboardError> {
    let Some(bytes) = powershell(
        "$ErrorActionPreference = 'Stop'; [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); $text = Get-Clipboard -Raw; if ($null -ne $text) { [Console]::Out.Write($text) }",
    ) else {
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

/// The clipboard image, as PNG bytes.
pub(super) fn get_image() -> Result<ClipboardRead<Vec<u8>>, ClipboardError> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let encoded = powershell(
        "$ErrorActionPreference = 'Stop'; Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing;
         $image = [Windows.Forms.Clipboard]::GetImage(); if ($null -ne $image) {
         $stream = New-Object IO.MemoryStream; $image.Save($stream, [Drawing.Imaging.ImageFormat]::Png);
         [Console]::Write([Convert]::ToBase64String($stream.ToArray())) }",
    ).ok_or(ClipboardError)?;
    if encoded.is_empty() {
        return Ok(ClipboardRead::Empty);
    }
    STANDARD
        .decode(encoded)
        .map(ClipboardRead::Data)
        .map_err(|_| ClipboardError)
}

/// The paths of the files on the clipboard.
pub(super) fn get_file_paths() -> Result<ClipboardRead<Vec<String>>, ClipboardError> {
    let Some(bytes) = powershell(
        "$ErrorActionPreference = 'Stop'; \
         [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); Get-Clipboard -Format FileDropList | ForEach-Object { [Console]::Out.WriteLine($_) }",
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

/// Passes text on standard input so PowerShell treats it as clipboard data.
pub(super) fn set_text(text: &str) -> Result<(), ClipboardError> {
    let mut child = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-STA", "-Command",
               "$ErrorActionPreference = 'Stop'; [Console]::InputEncoding = [Text.Encoding]::UTF8; Set-Clipboard -Value ([Console]::In.ReadToEnd())"])
        .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null())
        .spawn().map_err(|_| ClipboardError)?;
    let write = child
        .stdin
        .take()
        .ok_or(ClipboardError)?
        .write_all(text.as_bytes());
    let status = child.wait().map_err(|_| ClipboardError)?;
    if write.is_ok() && status.success() {
        Ok(())
    } else {
        Err(ClipboardError)
    }
}

/// Whether the modifier key is held down right now.
pub(super) fn is_modifier_pressed(modifier: ModifierKey) -> bool {
    let pressed = |key: u16| {
        // SAFETY: all callers pass documented Win32 virtual-key codes.
        (unsafe { GetAsyncKeyState(i32::from(key)) } & KEY_PRESSED_MASK) != 0
    };
    match modifier {
        ModifierKey::Shift => pressed(VK_SHIFT) || pressed(VK_LSHIFT) || pressed(VK_RSHIFT),
        ModifierKey::Control => pressed(VK_CONTROL) || pressed(VK_LCONTROL) || pressed(VK_RCONTROL),
        ModifierKey::Option => pressed(VK_MENU) || pressed(VK_LMENU) || pressed(VK_RMENU),
        ModifierKey::Command => pressed(VK_LWIN) || pressed(VK_RWIN),
    }
}
