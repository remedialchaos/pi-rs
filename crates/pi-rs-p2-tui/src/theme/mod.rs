pub mod builtin_themes;
#[path = "theme.rs"]
mod palette;
pub mod ref_theme;
mod syntax;
pub mod system_theme;
pub mod theme_schema;
pub mod theme_tokens;

pub use builtin_themes::{DARK_THEME, LIGHT_THEME};
pub use palette::{
    BUILTIN_THEMES, ResolveThemeOptions, ResolveThemeResult, SYSTEM_THEME_NAME, TerminalState,
    TerminalTheme, Theme, ThemeAppearance, ThemeError, ThemeStyle, create_system_theme,
    create_theme, detect_terminal_theme, parse_auto_theme_setting, parse_terminal_state,
    resolve_theme, resolve_theme_setting,
};
pub use ref_theme::ThemeRef;
pub use system_theme::{
    SystemThemeColors, SystemThemeInput, generate_system_theme_colors, indexed_colors,
    terminal_appearance,
};
pub use theme_schema::{
    ColorValue, ThemeColorValue, ThemeValidationError, ValidatedThemeJson, validate_theme_json,
};
pub use theme_tokens::{
    ResolvedThemeColorValues, THEME_TOKENS, ThemeBg, ThemeColor, ThemeColorValues, ThemeToken,
    ThemeTokenDescriptor, ThemeTokenDescriptors, ThemeTokenSlot, define_theme_tokens,
};

#[derive(Debug, thiserror::Error)]
pub enum ThemeFileError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Validation(#[from] ThemeValidationError),
    #[error(transparent)]
    Theme(#[from] ThemeError),
    #[error(transparent)]
    Watch(#[from] notify::Error),
}

/// Watch the containing directory so replacement by atomic rename also reloads the theme.
pub fn watch_theme_file(
    path: &std::path::Path,
    mode: pi_rs_tui::TerminalColorMode,
    terminal: TerminalState,
    mut changed: impl FnMut(Result<Theme, ThemeFileError>) + Send + 'static,
) -> Result<(Theme, notify::RecommendedWatcher), ThemeFileError> {
    use notify::Watcher;
    let path = std::path::absolute(path)?;
    let directory = path
        .parent()
        .ok_or_else(|| std::io::Error::other("theme file has no parent directory"))?
        .to_owned();
    let load = move |path: &std::path::Path| -> Result<Theme, ThemeFileError> {
        use std::io::Read;
        let limit = crate::MAX_PREFERENCE_FILE;
        if !std::fs::metadata(path)?.is_file() {
            return Err(std::io::Error::other("theme must be a regular file").into());
        }
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(std::io::Error::other("theme exceeds preference size limit").into());
        }
        let json = serde_json::from_slice(&bytes)?;
        let palette = validate_theme_json(&path.display().to_string(), &json)?;
        Ok(create_theme(&palette, mode, terminal.clone())?)
    };
    let selected = path.clone();
    let reload = load.clone();
    let mut watcher =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            Ok(event)
                if !matches!(event.kind, notify::EventKind::Access(_))
                    && event.paths.contains(&selected) =>
            {
                changed(reload(&selected))
            }
            Err(error) => changed(Err(error.into())),
            _ => {}
        })?;
    watcher.watch(&directory, notify::RecursiveMode::NonRecursive)?;
    Ok((load(&path)?, watcher))
}
