// eSpeak NG's C library is loaded only when Kokoro is enabled.
#![expect(
    unsafe_code,
    reason = "eSpeak NG exposes a C ABI and process-global phonemizer state"
)]

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::{LazyLock, Mutex};

use crate::{Error, Result};

static ESPEAK: LazyLock<Mutex<Option<Espeak>>> = LazyLock::new(|| Mutex::new(None));
const PUNCTUATION: &str = ";:,.!?¡¿—…\"«»“”(){}[]";

type Initialize = unsafe extern "C" fn(c_int, c_int, *const c_char, c_int) -> c_int;
type SetVoice = unsafe extern "C" fn(*const c_char) -> c_int;
type Phonemes = unsafe extern "C" fn(*mut *const c_void, c_int, c_int) -> *const c_char;

struct Espeak {
    _library: libloading::Library,
    set_voice: SetVoice,
    phonemes: Phonemes,
}

impl Espeak {
    fn open() -> Result<Self> {
        let name = std::env::var_os("PHONEMIZER_ESPEAK_LIBRARY").unwrap_or_else(|| {
            if cfg!(target_os = "windows") {
                "libespeak-ng.dll"
            } else if cfg!(target_os = "macos") {
                "libespeak-ng.dylib"
            } else {
                "libespeak-ng.so.1"
            }
            .into()
        });
        // SAFETY: `name` designates the administrator-selected eSpeak NG library. The
        // declarations below match speak_lib.h; the library is retained with its function pointers.
        unsafe {
            let library = libloading::Library::new(&name)
                .map_err(|e| Error::Config(format!("cannot load eSpeak NG ({name:?}): {e}")))?;
            let symbol =
                |e: libloading::Error| Error::Config(format!("invalid eSpeak NG library: {e}"));
            let initialize = *library
                .get::<Initialize>(b"espeak_Initialize\0")
                .map_err(symbol)?;
            let set_voice = *library
                .get::<SetVoice>(b"espeak_SetVoiceByName\0")
                .map_err(symbol)?;
            let phonemes = *library
                .get::<Phonemes>(b"espeak_TextToPhonemes\0")
                .map_err(symbol)?;
            // Retrieval (not audio playback), default data paths, and DONT_EXIT on error.
            if initialize(1, 0, std::ptr::null(), 0x8000) <= 0 {
                return Err(Error::Config(
                    "eSpeak NG initialization failed; install its voice data".into(),
                ));
            }
            Ok(Self {
                _library: library,
                set_voice,
                phonemes,
            })
        }
    }

    fn clause(&self, text: &str, out: &mut String) -> Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        let text = CString::new(text)
            .map_err(|_| Error::Request("Kokoro text contains a NUL byte".into()))?;
        let mut ptr = text.as_ptr().cast::<c_void>();
        while !ptr.is_null() {
            let previous = ptr;
            // SAFETY: the NUL-terminated UTF-8 buffer remains live until the call finishes;
            // eSpeak advances its pointer within that buffer or replaces it with NULL.
            // ESPEAK's mutex covers both the call and reading its result buffer.
            let clause = unsafe {
                let output = (self.phonemes)(&mut ptr, 1, 2);
                if output.is_null() {
                    return Err(Error::Model("eSpeak NG returned no phonemes".into()));
                }
                CStr::from_ptr(output).to_string_lossy()
            };
            let clause = clause.trim();
            if !clause.is_empty() {
                if !out.is_empty()
                    && !out.ends_with(' ')
                    && !PUNCTUATION.contains(out.chars().next_back().unwrap_or(' '))
                {
                    out.push(' ');
                }
                out.push_str(clause);
            }
            if ptr == previous {
                return Err(Error::Model("eSpeak NG did not consume input".into()));
            }
        }
        Ok(())
    }
}

pub(super) fn phonemize(text: &str, british: bool) -> Result<String> {
    let mut state = ESPEAK
        .lock()
        .map_err(|_| Error::Model("eSpeak lock poisoned".into()))?;
    if state.is_none() {
        *state = Some(Espeak::open()?);
    }
    let engine = state
        .as_ref()
        .ok_or_else(|| Error::Model("eSpeak is unavailable".into()))?;
    let voice = if british { c"en-gb" } else { c"en-us" };
    // SAFETY: voice is a static C string and this lock protects eSpeak NG's global state.
    if unsafe { (engine.set_voice)(voice.as_ptr()) } != 0 {
        return Err(Error::Model(format!("eSpeak NG cannot select {voice:?}")));
    }
    let mut out = String::with_capacity(text.len());
    let mut begin = 0;
    let bytes = text.as_bytes();
    for (index, c) in text.char_indices() {
        if !PUNCTUATION.contains(c) {
            continue;
        }
        if matches!(c, ',' | '.')
            && index > 0
            && bytes[index - 1].is_ascii_digit()
            && bytes.get(index + 1).is_some_and(u8::is_ascii_digit)
        {
            continue;
        }
        let before = &text[begin..index];
        if before.starts_with(char::is_whitespace) && !out.is_empty() && !out.ends_with(' ') {
            out.push(' ');
        }
        engine.clause(before, &mut out)?;
        if !out.is_empty() && before.ends_with(char::is_whitespace) && !out.ends_with(' ') {
            out.push(' ');
        }
        out.push(c);
        begin = index + c.len_utf8();
    }
    let tail = &text[begin..];
    if tail.starts_with(char::is_whitespace) && !out.is_empty() && !out.ends_with(' ') {
        out.push(' ');
    }
    engine.clause(tail, &mut out)?;
    Ok(out)
}
