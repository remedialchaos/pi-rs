use std::sync::mpsc;
use std::time::{Duration, Instant};

use pi_rs_tui::strip_terminal_sequences;
use serde_json::json;

use super::*;

fn custom() -> serde_json::Result<Value> {
    let mut theme: Value = serde_json::from_str(DARK)?;
    theme["name"] = json!("local");
    Ok(theme)
}

#[test]
fn builtins_are_distinct_and_markdown_closures_follow_selection() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let theme = Theme::load_with_mode(dir.path(), Some("dark"), TerminalColorMode::Truecolor)?;
    assert_eq!(theme.name(), "dark");
    assert_eq!(theme.names(), ["system", "dark", "light"]);
    let markdown = theme.markdown();
    let before = (markdown.heading)("heading");
    let revision = theme.revision();
    assert!(before.starts_with("\x1b[38;2;"));
    assert!(before.ends_with("heading\x1b[39m"));
    assert!(theme.bg("selectedBg", "row").starts_with("\x1b[48;2;"));
    assert_eq!(theme.bg("accent", "row"), "row");
    theme.select("light")?;
    assert_ne!(theme.revision(), revision);
    assert_eq!(theme.name(), "light");
    assert_ne!((markdown.heading)("heading"), before);
    assert_eq!(
        (markdown.heading)("heading"),
        theme.fg("mdHeading", "heading")
    );
    assert!(theme.select("anything/malformed/other").is_err());
    assert_eq!(theme.name(), "light");
    Ok(())
}

#[test]
fn optional_tokens_follow_fallbacks_and_variables_resolve() -> Result<(), Box<dyn Error>> {
    let mut value = custom()?;
    let colors = value["colors"].as_object_mut().ok_or("missing colors")?;
    for &(token, _) in FALLBACKS {
        colors.remove(token);
    }
    colors.insert("accent".into(), json!("first"));
    colors.insert("text".into(), json!(""));
    value["vars"]["first"] = json!("second");
    value["vars"]["second"] = json!(42);
    let palette = Palette::parse(&value.to_string(), "local", TerminalColorMode::Truecolor)?;
    let mut out = String::new();
    palette.append_fg(&mut out, "accent", "painted");
    assert_eq!(out, "\x1b[38;5;42mpainted\x1b[39m");
    for &(token, fallback) in FALLBACKS {
        assert_eq!(palette.paint.get(token), palette.paint.get(fallback));
    }
    assert_eq!(palette.paint["text"].open, "\x1b[39m");
    value["vars"]["second"] = json!("first");
    let error = Palette::parse(&value.to_string(), "local", TerminalColorMode::Truecolor)
        .err()
        .ok_or("cycle accepted")?;
    assert!(error.to_string().contains("Circular variable reference"));
    value["vars"]["second"] = json!("missing");
    let error = Palette::parse(&value.to_string(), "local", TerminalColorMode::Truecolor)
        .err()
        .ok_or("missing reference accepted")?;
    assert!(
        error
            .to_string()
            .contains("Variable reference not found: missing")
    );
    Ok(())
}

#[test]
fn custom_schema_rejects_invalid_document_shapes() -> Result<(), Box<dyn Error>> {
    let reject = |value: &Value| {
        Palette::parse(&value.to_string(), "bad", TerminalColorMode::Color256).is_err()
    };
    let mut value = custom()?;
    value["name"] = json!("bad/name");
    assert!(reject(&value));
    let mut value = custom()?;
    value["colors"]["typo"] = json!("#fff");
    assert!(reject(&value));
    let mut value = custom()?;
    value["colors"]
        .as_object_mut()
        .ok_or("missing colors")?
        .remove("accent");
    assert!(reject(&value));
    for input in [
        json!(-1),
        json!(256),
        json!(1.25),
        json!(null),
        json!(true),
        json!("#invalid"),
    ] {
        let mut value = custom()?;
        value["colors"]["accent"] = input;
        assert!(reject(&value));
    }
    let mut value = custom()?;
    value["export"]["typo"] = json!("#fff");
    assert!(reject(&value));
    Ok(())
}

#[test]
fn file_choices_use_document_name_and_reload_same_source() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("themes/on-disk.json");
    fs::create_dir_all(dir.path().join("themes"))?;
    let mut value = custom()?;
    fs::write(&path, value.to_string())?;
    let theme = Theme::load_with_mode(dir.path(), Some("local"), TerminalColorMode::Color256)?;
    assert_eq!(theme.names(), ["system", "dark", "light", "local"]);
    assert!(theme.fg("accent", "text").starts_with("\x1b[38;5;"));
    value["name"] = json!("renamed");
    fs::write(path, value.to_string())?;
    theme.reload()?;
    assert_eq!(theme.name(), "renamed");
    assert_eq!(theme.names(), ["system", "dark", "light", "renamed"]);
    Ok(())
}

#[test]
fn directory_watch_sees_atomic_replacement_and_retains_last_good_value()
-> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("themes/local.json");
    fs::create_dir_all(dir.path().join("themes"))?;
    let mut value = custom()?;
    value["colors"]["accent"] = json!(42);
    fs::write(&path, value.to_string())?;
    let theme = Theme::load_with_mode(dir.path(), Some("local"), TerminalColorMode::Truecolor)?;
    let clone = theme.clone();
    assert_eq!(theme.names(), ["system", "dark", "light", "local"]);
    let first = clone.fg("accent", "text");
    let revision = clone.revision();
    let (send, receive) = mpsc::channel();
    let _watcher = theme.watch(move || {
        let _ = send.send(());
    })?;
    let temp = dir.path().join("themes/replacement.tmp");
    value["colors"]["accent"] = json!(123);
    fs::write(&temp, value.to_string())?;
    #[cfg(windows)]
    fs::remove_file(&path)?;
    fs::rename(&temp, &path)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while clone.revision() == revision {
        receive.recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
        theme.reload()?;
    }
    assert_ne!(clone.fg("accent", "text"), first);
    assert_eq!(clone.fg("accent", "text"), "\x1b[38;5;123mtext\x1b[39m");
    let revision = clone.revision();
    fs::write(&path, "{")?;
    assert!(theme.reload().is_err());
    assert_eq!(clone.revision(), revision);
    fs::remove_file(&path)?;
    assert!(theme.reload().is_err());
    assert_eq!(clone.revision(), revision);
    Ok(())
}

#[test]
fn terminal_reports_drive_pairs_and_system_palette() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let theme =
        Theme::load_with_mode(dir.path(), Some("light/dark"), TerminalColorMode::Truecolor)?;
    theme.apply_terminal_scheme(TerminalColorScheme::Dark)?;
    assert_eq!(theme.name(), "dark");
    assert!(theme.apply_terminal_scheme(TerminalColorScheme::Light)?);
    assert_eq!(theme.name(), "light");
    assert!(theme.apply_terminal_response("\x1b]11;rgb:0000/0000/0000\x07")?);
    assert_eq!(theme.name(), "dark");
    assert!(!theme.apply_terminal_scheme(TerminalColorScheme::Light)?);
    assert!(!theme.apply_terminal_response("\x1b]11;malformed\x07")?);
    theme.select("system")?;
    assert!(theme.fg("accent", "x").starts_with("\x1b[38;2;"));
    assert!(theme.bg("selectedBg", "x").starts_with("\x1b[48;2;"));
    let revision = theme.revision();
    for index in 0..16 {
        let changed =
            theme.apply_terminal_response(&format!("\x1b]4;{index};rgb:ffff/0000/0000\x1b\\"))?;
        assert_eq!(changed, index == 15);
    }
    assert_ne!(theme.revision(), revision);
    assert!(!theme.apply_terminal_colors(TerminalColors::default())?);
    Ok(())
}

#[test]
fn no_terminal_reply_switches_from_grayscale_to_indexed() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let theme = Theme::load_with_mode(dir.path(), None, TerminalColorMode::Truecolor)?;
    assert_eq!(theme.fg("accent", "x"), "\x1b[39mx\x1b[39m");
    assert!(theme.fg("muted", "x").contains("\x1b[2m"));
    assert!(theme.apply_terminal_colors(TerminalColors::default())?);
    assert_eq!(theme.fg("accent", "x"), "\x1b[38;5;5mx\x1b[39m");
    assert_eq!(theme.bg("selectedBg", "x"), "\x1b[49mx\x1b[49m");
    assert!(!theme.apply_terminal_colors(TerminalColors::default())?);
    assert_eq!(env_scheme("15;default;7"), Some(TerminalColorScheme::Light));
    assert_eq!(env_scheme("0;8"), Some(TerminalColorScheme::Dark));
    assert_eq!(env_scheme("0;default"), None);
    Ok(())
}

#[test]
fn syntax_highlights_known_languages_without_guessing_unknown_ones() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let theme = Theme::load_with_mode(dir.path(), Some("dark"), TerminalColorMode::Truecolor)?;
    let code = "fn test() {\n    // café\n    let x = 42;\n}\n";
    let rendered = theme.highlight_code(code, Some("rust"));
    assert_eq!(rendered.len(), 5);
    assert!(rendered[0].contains(&theme.fg("syntaxFunction", "test")));
    assert!(rendered[2].contains(&theme.fg("syntaxNumber", "42")));
    assert_eq!(strip_terminal_sequences(&rendered.join("\n")), code);
    assert_eq!(
        theme.highlight_code("fn test", Some("not-a-language")),
        [theme.fg("mdCodeBlock", "fn test")]
    );
    Ok(())
}

#[test]
fn system_recipe_paints_every_token_even_on_mid_gray() -> Result<(), ThemeError> {
    let schema = THEME_SCHEMA.as_ref().map_err(Clone::clone)?;
    for channel in [0.0, 115.0, 119.0, 255.0] {
        let terminal = TerminalColors {
            foreground: None,
            background: Some(RgbColor {
                r: channel,
                g: channel,
                b: channel,
            }),
            palette: None,
        };
        let colors: HashMap<_, _> = system::generate(&terminal, false)
            .map(|(token, color, dim)| (token, (color, dim)))
            .collect();
        assert_eq!(colors.len(), schema.tokens.len());
        for token in &schema.tokens {
            assert!(
                matches!(
                    colors.get(token.as_str()),
                    Some((Some(Color::Rgb(_)), false))
                ),
                "{token}"
            );
        }
    }
    Ok(())
}

#[test]
fn native_colors_follow_theme_and_terminal_defaults() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let theme = Theme::load_with_mode(dir.path(), None, TerminalColorMode::Truecolor)?;
    theme.apply_terminal_scheme(TerminalColorScheme::Light)?;
    assert_eq!(theme.appearance(), TerminalColorScheme::Light);
    let white = RgbColor {
        r: 255.0,
        g: 255.0,
        b: 255.0,
    };
    assert_eq!(theme.color("selectedBg"), Some(Color::Rgb(white)));
    assert_eq!(theme.color("not-a-token"), None);
    let background = RgbColor {
        r: 8.0,
        g: 8.0,
        b: 8.0,
    };
    theme.apply_terminal_colors(TerminalColors {
        background: Some(background),
        ..TerminalColors::default()
    })?;
    assert_eq!(theme.appearance(), TerminalColorScheme::Dark);
    theme.select("light")?;
    assert_eq!(theme.appearance(), TerminalColorScheme::Light);
    assert!(theme.color("accent").is_some());
    Ok(())
}
