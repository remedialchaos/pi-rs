//! Format the available chat model catalog for `--list-models`.

use std::cmp::Ordering;
use std::fmt::Write;
use std::path::Path;

use pi_rs_ai::{Model, ModelInputModality};
use pi_rs_tui::fuzzy::fuzzy_filter;

const HEADERS: [&str; 6] = [
    "provider", "model", "context", "max-out", "thinking", "images",
];

// Printable ASCII collation in the upstream default locale. Letters have the
// same primary weight regardless of case, with lowercase ordered first on ties.
const ASCII_PRIMARY: [u8; 128] = {
    let weights = b" _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$0123456789abcdefghijklmnopqrstuvwxyz";
    let mut result = [0; 128];
    let mut index = 0;
    while index < weights.len() {
        result[weights[index] as usize] = index as u8 + 1;
        index += 1;
    }
    result
};

fn identifier_cmp(left: &str, right: &str) -> Ordering {
    fn weights(value: &str) -> impl Iterator<Item = u32> + '_ {
        value.chars().flat_map(char::to_lowercase).map(|c| {
            if c.is_ascii() {
                u32::from(ASCII_PRIMARY[c as usize])
            } else {
                c as u32
            }
        })
    }

    weights(left).cmp(weights(right)).then_with(|| {
        left.chars()
            .map(char::is_uppercase)
            .cmp(right.chars().map(char::is_uppercase))
    })
}

fn format_token_count(count: u64) -> String {
    let (scale, unit) = match count {
        1_000_000.. => (1_000_000.0, 'M'),
        1_000.. => (1_000.0, 'K'),
        _ => return count.to_string(),
    };
    let value = count as f64 / scale;
    match value.fract() {
        0.0 => format!("{value}{unit}"),
        // JS rounds a true midpoint up. Rust rounds 0.25 to the even tenth;
        // 0.75 already rounds the same way in both languages.
        0.25 => format!("{}.3{unit}", value.trunc()),
        _ => format!("{value:.1}{unit}"),
    }
}

fn write_row(output: &mut String, cells: [&str; 6], widths: [usize; 6]) {
    for (index, (cell, width)) in cells.into_iter().zip(widths).enumerate() {
        if index > 0 {
            output.push_str("  ");
        }
        output.push_str(cell);
        let _ = write!(
            output,
            "{:padding$}",
            "",
            padding = width - cell.encode_utf16().count()
        );
    }
    output.push('\n');
}

/// Filter and format chat models that the runtime has already resolved as
/// available. The returned stdout includes each table row's trailing padding.
pub fn format_models(models: &[Model], search_pattern: Option<&str>, docs_dir: &Path) -> String {
    if models.is_empty() {
        return format!(
            "No models available. Use /login to log into a provider via OAuth or API key. See:\n  {}\n  {}\n",
            docs_dir.join("providers.md").display(),
            docs_dir.join("models.md").display(),
        );
    }

    let mut selected: Vec<_> = models.iter().collect();
    if let Some(pattern) = search_pattern.filter(|pattern| !pattern.is_empty()) {
        selected = fuzzy_filter(&selected, pattern, |model| {
            format!("{} {}", model.base.provider, model.base.id)
        });
        if selected.is_empty() {
            return format!("No models matching \"{pattern}\"\n");
        }
    }
    selected.sort_by(|left, right| {
        identifier_cmp(&left.base.provider, &right.base.provider)
            .then_with(|| identifier_cmp(&left.base.id, &right.base.id))
    });

    let mut widths = HEADERS.map(str::len);
    let rows: Vec<_> = selected
        .into_iter()
        .map(|model| {
            let context = format_token_count(model.context_window);
            let max_out = format_token_count(model.max_tokens);
            widths[0] = widths[0].max(model.base.provider.encode_utf16().count());
            widths[1] = widths[1].max(model.base.id.encode_utf16().count());
            widths[2] = widths[2].max(context.len());
            widths[3] = widths[3].max(max_out.len());
            (model, context, max_out)
        })
        .collect();
    let row_width = widths.iter().sum::<usize>() + 11;
    let mut output = String::with_capacity(row_width * (rows.len() + 1));
    write_row(&mut output, HEADERS, widths);
    for (model, context, max_out) in rows {
        write_row(
            &mut output,
            [
                &model.base.provider,
                &model.base.id,
                &context,
                &max_out,
                if model.reasoning { "yes" } else { "no" },
                if model.base.input.contains(&ModelInputModality::Image) {
                    "yes"
                } else {
                    "no"
                },
            ],
            widths,
        );
    }
    output
}
