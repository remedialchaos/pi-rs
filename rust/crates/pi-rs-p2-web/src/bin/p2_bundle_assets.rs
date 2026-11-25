#![forbid(unsafe_code)]

//! Pure-Rust CSS and WOFF2 font bundler for `p2` browser assets.
//!
//! Resolves `@import` chains starting at `browser.css`, rewrites vendored
//! `./files/<name>.woff2` font URLs to `./<name>.woff2`, writes `p2.css`,
//! copies the WOFF2 fonts alongside it, and verifies invariants without
//! Node.js or `esbuild`.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

const REQUIRED_FONTS: &[&str] = &[
    "CommitMono-300-Regular.woff2",
    "CommitMono-400-Regular.woff2",
    "CommitMono-700-Regular.woff2",
    "MallosSerif-Italic-Variable.woff2",
    "MallosSerif-Variable.woff2",
];

struct CliArgs {
    out_dir: PathBuf,
    assets_dir: PathBuf,
}

fn default_assets_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets")
}

fn parse_args<I>(mut args: I) -> Result<CliArgs, String>
where
    I: Iterator<Item = String>,
{
    let _program = args.next();
    let mut out_dir: Option<PathBuf> = None;
    let mut assets_dir: Option<PathBuf> = None;

    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--out-dir=") {
            out_dir = Some(PathBuf::from(value));
        } else if arg == "--out-dir" {
            let value = args
                .next()
                .ok_or_else(|| "missing value for --out-dir".to_owned())?;
            out_dir = Some(PathBuf::from(value));
        } else if let Some(value) = arg.strip_prefix("--assets-dir=") {
            assets_dir = Some(PathBuf::from(value));
        } else if arg == "--assets-dir" {
            let value = args
                .next()
                .ok_or_else(|| "missing value for --assets-dir".to_owned())?;
            assets_dir = Some(PathBuf::from(value));
        } else {
            return Err(format!("unexpected argument: {arg}"));
        }
    }

    let out_dir = out_dir.ok_or_else(|| "missing required --out-dir <DIR> argument".to_owned())?;
    if out_dir.as_os_str().is_empty() {
        return Err("--out-dir must not be empty".to_owned());
    }
    let assets_dir = assets_dir.unwrap_or_else(default_assets_dir);
    if assets_dir.as_os_str().is_empty() {
        return Err("--assets-dir must not be empty".to_owned());
    }

    Ok(CliArgs {
        out_dir,
        assets_dir,
    })
}

#[derive(Default)]
struct BundleState {
    visited: BTreeSet<PathBuf>,
    active_stack: Vec<PathBuf>,
    fonts: BTreeMap<String, PathBuf>,
    output: String,
}

fn bundle_p2_assets(assets_dir: &Path, out_dir: &Path) -> Result<BTreeSet<String>, String> {
    let copied = bundle_css(assets_dir, out_dir)?;
    for required in REQUIRED_FONTS {
        if !copied.contains(*required) {
            return Err(format!("missing required vendored font: {required}"));
        }
    }
    Ok(copied)
}

fn bundle_css(assets_dir: &Path, out_dir: &Path) -> Result<BTreeSet<String>, String> {
    let entry = assets_dir.join("browser.css");
    let mut state = BundleState::default();
    process_css_file(&entry, &mut state)?;

    if has_unresolved_import(&state.output) {
        return Err("bundled p2.css still contains an unresolved @import".to_owned());
    }

    fs::create_dir_all(out_dir)
        .map_err(|err| format!("failed to create {}: {err}", out_dir.display()))?;
    let out_css_path = out_dir.join("p2.css");
    fs::write(&out_css_path, state.output.as_bytes())
        .map_err(|err| format!("failed to write {}: {err}", out_css_path.display()))?;

    let mut copied = BTreeSet::new();
    for (font_name, src_path) in &state.fonts {
        let bytes = fs::read(src_path)
            .map_err(|err| format!("failed to read font {}: {err}", src_path.display()))?;
        if !bytes.starts_with(b"wOF2") {
            return Err(format!(
                "font {} does not start with wOF2 signature",
                src_path.display()
            ));
        }
        let dest_path = out_dir.join(font_name);
        fs::write(&dest_path, &bytes)
            .map_err(|err| format!("failed to write font {}: {err}", dest_path.display()))?;
        let written = fs::read(&dest_path)
            .map_err(|err| format!("failed to verify font {}: {err}", dest_path.display()))?;
        if !written.starts_with(b"wOF2") {
            return Err(format!(
                "copied font {} failed wOF2 verification",
                dest_path.display()
            ));
        }
        let expected_ref = format!("./{font_name}");
        if !state.output.contains(&expected_ref) {
            return Err(format!(
                "bundled p2.css does not reference copied font {expected_ref}"
            ));
        }
        copied.insert(font_name.clone());
    }

    Ok(copied)
}

fn process_css_file(path: &Path, state: &mut BundleState) -> Result<(), String> {
    let canonical = path
        .canonicalize()
        .map_err(|err| format!("cannot resolve CSS file {}: {err}", path.display()))?;
    if state.active_stack.contains(&canonical) {
        return Err(format!(
            "cyclic @import detected at {}",
            canonical.display()
        ));
    }
    if !state.visited.insert(canonical.clone()) {
        return Ok(());
    }
    state.active_stack.push(canonical.clone());

    let content = fs::read_to_string(&canonical)
        .map_err(|err| format!("failed to read CSS file {}: {err}", canonical.display()))?;
    let base_dir = canonical
        .parent()
        .ok_or_else(|| format!("CSS file has no parent directory: {}", canonical.display()))?;

    let result = process_css_contents(&content, base_dir, state);
    state.active_stack.pop();
    result
}

fn process_css_contents(
    content: &str,
    base_dir: &Path,
    state: &mut BundleState,
) -> Result<(), String> {
    let mut cursor = 0usize;
    while cursor < content.len() {
        if content[cursor..].starts_with("/*") {
            let end = content[cursor + 2..]
                .find("*/")
                .map_or(content.len(), |offset| cursor + 2 + offset + 2);
            state.output.push_str(&content[cursor..end]);
            cursor = end;
            continue;
        }

        if starts_with_at_import(&content[cursor..]) {
            let (specifier, next_cursor) = parse_import_statement(content, cursor)?;
            let target = base_dir.join(specifier);
            process_css_file(&target, state)?;
            cursor = next_cursor;
            if !state.output.is_empty() && !state.output.ends_with('\n') {
                state.output.push('\n');
            }
            continue;
        }

        if let Some((font_name, end_offset)) = parse_files_woff2_url(&content[cursor..]) {
            validate_font_filename(&font_name)?;
            let src_path = base_dir
                .join("files")
                .join(&font_name)
                .canonicalize()
                .map_err(|err| {
                    format!(
                        "missing font asset {} referenced from {}: {err}",
                        base_dir.join("files").join(&font_name).display(),
                        base_dir.display()
                    )
                })?;
            if let Some(existing) = state.fonts.get(&font_name)
                && existing != &src_path
            {
                return Err(format!(
                    "conflicting font sources for {font_name}: {} vs {}",
                    existing.display(),
                    src_path.display()
                ));
            }
            state.fonts.insert(font_name.clone(), src_path);
            state.output.push_str(&format!("url(\"./{font_name}\")"));
            cursor += end_offset;
            continue;
        }

        let Some(ch) = content[cursor..].chars().next() else {
            break;
        };
        state.output.push(ch);
        cursor += ch.len_utf8();
    }
    Ok(())
}

fn starts_with_at_import(slice: &str) -> bool {
    let Some(rest) = slice.strip_prefix("@import") else {
        return false;
    };
    rest.chars()
        .next()
        .is_some_and(|ch| ch.is_whitespace() || ch == '"' || ch == '\'')
}

fn parse_import_statement(content: &str, start: usize) -> Result<(&str, usize), String> {
    let rest = &content[start + "@import".len()..];
    let after_ws = rest.trim_start();
    let trimmed_offset = rest.len() - after_ws.len();
    let mut pos = start + "@import".len() + trimmed_offset;

    let mut has_url_wrapper = false;
    if content[pos..].starts_with("url(") {
        has_url_wrapper = true;
        pos += "url(".len();
        let inner = content[pos..].trim_start();
        pos += content[pos..].len() - inner.len();
    }

    let quote = content[pos..]
        .chars()
        .next()
        .filter(|&ch| ch == '"' || ch == '\'')
        .ok_or_else(|| format!("unsupported @import syntax near byte {start}"))?;
    pos += quote.len_utf8();

    let spec_end_rel = content[pos..]
        .find(quote)
        .ok_or_else(|| format!("unterminated @import string near byte {start}"))?;
    let specifier = &content[pos..pos + spec_end_rel];
    pos += spec_end_rel + quote.len_utf8();

    let tail = content[pos..].trim_start();
    pos += content[pos..].len() - tail.len();

    if has_url_wrapper {
        if !content[pos..].starts_with(')') {
            return Err(format!(
                "missing closing ')' in @import url(...) near byte {start}"
            ));
        }
        pos += 1;
        let tail = content[pos..].trim_start();
        pos += content[pos..].len() - tail.len();
    }

    if !content[pos..].starts_with(';') {
        return Err(format!(
            "unsupported or missing ';' in @import {specifier:?} near byte {start}"
        ));
    }
    pos += 1;
    if content[pos..].starts_with("\r\n") {
        pos += 2;
    } else if content[pos..].starts_with('\n') {
        pos += 1;
    }

    Ok((specifier, pos))
}

fn parse_files_woff2_url(slice: &str) -> Option<(String, usize)> {
    let inner = slice.strip_prefix("url(")?;
    let trimmed = inner.trim_start();
    let ws_before = inner.len() - trimmed.len();
    let quote = trimmed.chars().next().filter(|&c| c == '"' || c == '\'')?;
    let after_quote = &trimmed[quote.len_utf8()..];
    let path_end = after_quote.find(quote)?;
    let url_path = &after_quote[..path_end];
    let after_path = &after_quote[path_end + quote.len_utf8()..];
    let after_ws = after_path.trim_start();
    let ws_after = after_path.len() - after_ws.len();
    if !after_ws.starts_with(')') {
        return None;
    }
    let font_rel = url_path.strip_prefix("./files/")?;
    if !font_rel.ends_with(".woff2") {
        return None;
    }
    let total_len =
        "url(".len() + ws_before + quote.len_utf8() + path_end + quote.len_utf8() + ws_after + 1;
    Some((font_rel.to_owned(), total_len))
}

fn validate_font_filename(name: &str) -> Result<(), String> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Err(format!("invalid font filename: {name:?}"));
    }
    Ok(())
}

fn has_unresolved_import(css: &str) -> bool {
    if css
        .lines()
        .any(|line| line.trim_start().starts_with("@import"))
    {
        return true;
    }
    let mut cursor = 0usize;
    while cursor < css.len() {
        if css[cursor..].starts_with("/*") {
            cursor = css[cursor + 2..]
                .find("*/")
                .map_or(css.len(), |offset| cursor + 2 + offset + 2);
            continue;
        }
        if starts_with_at_import(&css[cursor..]) {
            return true;
        }
        let Some(ch) = css[cursor..].chars().next() else {
            break;
        };
        cursor += ch.len_utf8();
    }
    false
}

fn main() -> ExitCode {
    let args = match parse_args(env::args()) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("p2-bundle-assets: {err}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(err) = bundle_p2_assets(&args.assets_dir, &args.out_dir) {
        eprintln!("p2-bundle-assets: {err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TMP_ID: AtomicU64 = AtomicU64::new(1);

    struct ScratchDir {
        path: PathBuf,
    }

    impl ScratchDir {
        fn new(label: &str) -> Result<Self, String> {
            let seq = NEXT_TMP_ID.fetch_add(1, Ordering::Relaxed);
            let path =
                env::temp_dir().join(format!("pi-rs-p2-web-{label}-{}-{seq}", std::process::id()));
            fs::create_dir_all(&path)
                .map_err(|err| format!("failed to create {}: {err}", path.display()))?;
            Ok(Self { path })
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn bundles_real_p2_assets_with_all_five_woff2_fonts_and_zero_imports() -> Result<(), String> {
        let out = ScratchDir::new("real")?;
        let copied = bundle_p2_assets(&default_assets_dir(), &out.path)?;
        let expected: BTreeSet<String> = REQUIRED_FONTS.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(copied, expected);

        let p2_css = fs::read_to_string(out.path.join("p2.css"))
            .map_err(|err| format!("failed to read p2.css: {err}"))?;
        assert!(!has_unresolved_import(&p2_css));
        assert!(!p2_css.contains("./files/"));

        let layer_pos = p2_css
            .find("@layer p2-reset, elanor;")
            .ok_or("missing @layer declaration")?;
        let markdown_pos = p2_css
            .find(".elanor-markdown")
            .ok_or("missing elanor-markdown rules")?;
        let style_pos = p2_css.find("--bg").ok_or("missing style.css tokens")?;
        assert!(layer_pos < markdown_pos);
        assert!(markdown_pos < style_pos);

        for font in REQUIRED_FONTS {
            assert!(
                p2_css.contains(&format!("url(\"./{font}\")")),
                "p2.css missing rewritten url for {font}"
            );
            let vendored = fs::read(default_assets_dir().join("vendor/elanor/files").join(font))
                .map_err(|err| format!("missing vendored font {font}: {err}"))?;
            let bundled = fs::read(out.path.join(font))
                .map_err(|err| format!("missing bundled font {font}: {err}"))?;
            assert!(bundled.starts_with(b"wOF2"));
            assert_eq!(bundled, vendored);
        }
        Ok(())
    }

    #[test]
    fn resolves_nested_relative_imports_in_order_and_deduplicates_repeats() -> Result<(), String> {
        let root = ScratchDir::new("nested")?;
        let assets = root.path.join("assets");
        let sub = assets.join("sub");
        let files = sub.join("files");
        let out = root.path.join("out");
        fs::create_dir_all(&files).map_err(|err| err.to_string())?;

        fs::write(
            assets.join("browser.css"),
            "@import \"./sub/a.css\";\n@import './sub/b.css';\n.tail { color: red; }\n",
        )
        .map_err(|err| err.to_string())?;
        fs::write(
            sub.join("a.css"),
            "@import \"../shared.css\";\n@font-face { src: url('./files/Alpha.woff2') format('woff2'); }\n",
        )
        .map_err(|err| err.to_string())?;
        fs::write(
            sub.join("b.css"),
            "@import url(\"../shared.css\");\n@font-face { src: url(\"./files/Beta.woff2\") format('woff2'); }\n",
        )
        .map_err(|err| err.to_string())?;
        fs::write(assets.join("shared.css"), ".shared { display: block; }\n")
            .map_err(|err| err.to_string())?;
        fs::write(files.join("Alpha.woff2"), b"wOF2alpha").map_err(|err| err.to_string())?;
        fs::write(files.join("Beta.woff2"), b"wOF2beta").map_err(|err| err.to_string())?;

        let copied = bundle_css(&assets, &out)?;
        assert_eq!(
            copied,
            BTreeSet::from(["Alpha.woff2".to_owned(), "Beta.woff2".to_owned()])
        );
        let p2_css = fs::read_to_string(out.join("p2.css")).map_err(|err| err.to_string())?;
        assert!(!has_unresolved_import(&p2_css));
        assert_eq!(p2_css.matches(".shared { display: block; }").count(), 1);
        assert!(p2_css.contains("url(\"./Alpha.woff2\")"));
        assert!(p2_css.contains("url(\"./Beta.woff2\")"));
        let shared_pos = p2_css.find(".shared").ok_or("missing .shared")?;
        let alpha_pos = p2_css.find("Alpha.woff2").ok_or("missing Alpha")?;
        let beta_pos = p2_css.find("Beta.woff2").ok_or("missing Beta")?;
        let tail_pos = p2_css.find(".tail").ok_or("missing .tail")?;
        assert!(shared_pos < alpha_pos && alpha_pos < beta_pos && beta_pos < tail_pos);
        Ok(())
    }

    #[test]
    fn rejects_invalid_woff2_header_missing_import_and_import_cycles() -> Result<(), String> {
        let root = ScratchDir::new("errors")?;
        let assets = root.path.join("assets");
        let files = assets.join("files");
        let out = root.path.join("out");
        fs::create_dir_all(&files).map_err(|err| err.to_string())?;

        fs::write(assets.join("browser.css"), "@import \"./missing.css\";\n")
            .map_err(|err| err.to_string())?;
        let missing_err = bundle_css(&assets, &out).expect_err("missing import must fail");
        assert!(missing_err.contains("cannot resolve CSS file"));

        fs::write(assets.join("browser.css"), "@import \"./cycle.css\";\n")
            .map_err(|err| err.to_string())?;
        fs::write(assets.join("cycle.css"), "@import \"./browser.css\";\n")
            .map_err(|err| err.to_string())?;
        let cycle_err = bundle_css(&assets, &out).expect_err("import cycle must fail");
        assert!(cycle_err.contains("cyclic @import"));

        fs::write(
            assets.join("browser.css"),
            "@font-face { src: url('./files/Bad.woff2'); }\n",
        )
        .map_err(|err| err.to_string())?;
        fs::write(files.join("Bad.woff2"), b"NOTWOFF2").map_err(|err| err.to_string())?;
        let magic_err = bundle_css(&assets, &out).expect_err("bad wOFF2 header must fail");
        assert!(magic_err.contains("wOF2"));
        Ok(())
    }
}
