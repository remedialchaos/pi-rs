use super::{InlineStyle, Kind, Markdown, Node, Prepared, Tag};
use crate::utils::{visible_width, wrap_text_with_ansi};

const ROW_FRAGMENT_RESET: &str = "\x1b[22;23;24;25;27;28;29;39m";

fn distribute(sizes: &mut [usize], targets: &[usize], max: usize) {
    let remaining = max.saturating_sub(sizes.iter().sum());
    let total: usize = targets
        .iter()
        .zip(sizes.iter())
        .map(|(end, start)| end.saturating_sub(*start))
        .sum();
    if total == 0 || remaining == 0 {
        return;
    }
    let mut left = remaining;
    for (size, target) in sizes.iter_mut().zip(targets) {
        let want = target.saturating_sub(*size);
        let share = ((want as f64 / total as f64) * remaining as f64) as usize;
        let share = share.min(want).min(left);
        *size += share;
        left -= share;
    }
    while left > 0 {
        let before = left;
        for (size, &target) in sizes.iter_mut().zip(targets) {
            if *size < target {
                *size += 1;
                left -= 1;
                if left == 0 {
                    break;
                }
            }
        }
        if before == left {
            break;
        }
    }
}

impl Markdown {
    pub(super) fn table(
        &self,
        node: &Node<'_>,
        columns: usize,
        width: usize,
        source: &Prepared<'_>,
        style: &InlineStyle<'_>,
    ) -> Vec<String> {
        if columns == 0 {
            return Vec::new();
        }
        let cells = width.saturating_sub(columns * 3 + 1);
        if cells < columns {
            return wrap_text_with_ansi(&source.restore(&source.text[node.raw.clone()]), width);
        }
        let mut natural = vec![1; columns];
        let mut minimum = natural.clone();
        let rows: Vec<_> = node
            .children
            .iter()
            .filter(|row| matches!(row.kind, Kind::Tag(Tag::TableHead | Tag::TableRow)))
            .map(|row| {
                row.children
                    .iter()
                    .take(columns)
                    .enumerate()
                    .map(|(i, cell)| {
                        let text = self.inline(&cell.children, source, style);
                        natural[i] = natural[i].max(visible_width(&text));
                        let longest = text
                            .split_whitespace()
                            .map(visible_width)
                            .max()
                            .unwrap_or(0);
                        minimum[i] = minimum[i].max(longest.min(30));
                        text
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut sizes = minimum.clone();
        if sizes.iter().sum::<usize>() > cells {
            sizes.fill(1);
            distribute(&mut sizes, &minimum, cells);
        }
        distribute(&mut sizes, &natural, cells);
        let rules = |start: &str, join: &str, end: &str| {
            let mut out = start.to_owned();
            for (i, &size) in sizes.iter().enumerate() {
                if i > 0 {
                    out.push_str(join);
                }
                out.push_str(&"─".repeat(size + 2));
            }
            out.push_str(end);
            out
        };
        let divider = rules("├", "┼", "┤");
        let mut lines = vec![rules("┌", "┬", "┐")];
        for (i, row) in rows.iter().enumerate() {
            let columns: Vec<_> = row
                .iter()
                .zip(&sizes)
                .map(|(cell, &size)| wrap_text_with_ansi(cell, size))
                .collect();
            let height = columns.iter().map(Vec::len).max().unwrap_or(1);
            for y in 0..height {
                let mut line = String::from("│ ");
                for (x, &size) in sizes.iter().enumerate() {
                    if x > 0 {
                        line.push_str(" │ ");
                    }
                    let wraps = columns.get(x);
                    let text = wraps
                        .and_then(|lines| lines.get(y))
                        .map_or("", String::as_str);
                    let mut cell = String::from(text);
                    if wraps.is_some_and(|rows| y + 1 < rows.len()) {
                        cell.push_str(ROW_FRAGMENT_RESET);
                    }
                    cell.push_str(&style.prefix);
                    cell.extend(std::iter::repeat_n(
                        ' ',
                        size.saturating_sub(visible_width(text)),
                    ));
                    if i == 0 {
                        line.push_str(&(self.theme.bold)(&cell));
                    } else {
                        line.push_str(&cell);
                    }
                }
                line.push_str(" │");
                lines.push(line);
            }
            if i == 0 || i + 1 < rows.len() {
                lines.push(divider.clone());
            }
        }
        lines.push(rules("└", "┴", "┘"));
        lines
    }
}
