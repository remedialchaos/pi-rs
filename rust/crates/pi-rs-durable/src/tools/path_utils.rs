use std::{borrow::Cow, sync::LazyLock};

use pi_rs_chord::context::Context;
use regex::Regex;

use crate::{
    env::{ExecutionEnv, FileError},
    unicode::nfd,
};

static MERIDIEM: LazyLock<Regex> = LazyLock::new(|| {
    // INFALLIBLE: fixed ASCII pattern.
    Regex::new(r" (?i-u:am|pm)\.").expect("valid meridiem pattern")
});

fn is_unicode_space(ch: char) -> bool {
    matches!(
        ch,
        '\u{a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

pub async fn resolve_tool_path(
    env: &dyn ExecutionEnv,
    path: &str,
    context: &Context,
) -> Result<String, FileError> {
    let path = path.strip_prefix('@').unwrap_or(path);
    let normalized = if path.contains(is_unicode_space) {
        Cow::Owned(path.replace(is_unicode_space, " "))
    } else {
        Cow::Borrowed(path)
    };
    env.absolute_path(&normalized, context).await
}

pub async fn resolve_read_tool_path(
    env: &dyn ExecutionEnv,
    path: &str,
    context: &Context,
) -> Result<String, FileError> {
    let resolved = resolve_tool_path(env, path, context).await?;
    if env.exists(&resolved, context).await? {
        return Ok(resolved);
    }

    let narrow = MERIDIEM.replace_all(&resolved, |capture: &regex::Captures<'_>| {
        format!("\u{202f}{}", &capture[0][1..])
    });
    if narrow != resolved && env.exists(&narrow, context).await? {
        return Ok(narrow.into_owned());
    }
    let decomposed = nfd(&resolved);
    if decomposed != resolved && decomposed != narrow && env.exists(&decomposed, context).await? {
        return Ok(decomposed.into_owned());
    }
    if resolved.contains('\'') {
        let curved = resolved.replace('\'', "\u{2019}");
        if curved != narrow && curved != decomposed && env.exists(&curved, context).await? {
            return Ok(curved);
        }
        let both = decomposed.replace('\'', "\u{2019}");
        if both != curved
            && both != narrow
            && both != decomposed
            && env.exists(&both, context).await?
        {
            return Ok(both);
        }
    }
    Ok(resolved)
}
