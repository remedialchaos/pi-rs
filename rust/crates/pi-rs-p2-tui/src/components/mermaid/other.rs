use std::borrow::Cow;

use indexmap::IndexMap;
use pi_rs_tui::visible_width;

use super::{Art, Role};

pub(super) fn sequence<'a>(
    lines: impl Iterator<Item = &'a str>,
    width: usize,
) -> Result<Option<Art>, &'static str> {
    let mut participants = IndexMap::new();
    let mut steps = Vec::new();
    let mut numbered = false;
    for line in lines.map(str::trim) {
        let (keyword, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        match keyword {
            "participant" | "actor" => {
                let (id, label) = rest
                    .trim()
                    .split_once(" as ")
                    .unwrap_or((rest.trim(), rest.trim()));
                if id.is_empty() {
                    return Err("missing participant name");
                }
                participants.insert(id, label.trim_matches('"'));
            }
            "autonumber" => numbered = true,
            "activate" | "deactivate" => {}
            "opt" | "loop" | "alt" | "else" | "par" | "and" | "critical" | "break" => {
                steps.push(SequenceStep::Divider(line));
            }
            "end" => steps.push(SequenceStep::Divider("end")),
            "Note" | "note" => {
                let (position, label) = rest.split_once(':').ok_or("note is missing its text")?;
                let participant = position
                    .split_whitespace()
                    .last()
                    .and_then(|value| value.split(',').next())
                    .ok_or("note is missing its participant")?;
                participants.entry(participant).or_insert(participant);
                steps.push(SequenceStep::Note(participant, label.trim()));
            }
            _ => {
                let (route, label) = line.split_once(':').ok_or("expected a sequence message")?;
                let (from, to, dashed, headed) = [
                    ("-->>", true, true),
                    ("->>", false, true),
                    ("-->", true, false),
                    ("->", false, false),
                    ("--x", true, true),
                    ("-x", false, true),
                ]
                .into_iter()
                .find_map(|(op, dashed, headed)| {
                    let (from, to) = route.split_once(op)?;
                    Some((
                        from.trim(),
                        to.trim().trim_start_matches(['+', '-']),
                        dashed,
                        headed,
                    ))
                })
                .ok_or("unsupported sequence arrow")?;
                if from.is_empty() || to.is_empty() {
                    return Err("missing message participant");
                }
                participants.entry(from).or_insert(from);
                participants.entry(to).or_insert(to);
                steps.push(SequenceStep::Message {
                    from,
                    to,
                    text: label.trim(),
                    dashed,
                    headed,
                });
            }
        }
    }
    if participants.is_empty() || participants.len() > 64 || steps.len() > 200 {
        return Ok(None);
    }
    let mut widths: Vec<_> = participants
        .values()
        .map(|label| visible_width(label) + 4)
        .collect();
    let positions = |widths: &[usize]| -> Vec<usize> {
        let mut offset = 0;
        widths
            .iter()
            .map(|&size| {
                let center = offset + size / 2;
                offset += size + 2;
                center
            })
            .collect()
    };
    for step in &steps {
        if let SequenceStep::Message { from, to, text, .. } = *step
            && let (Some(first), Some(last)) = (
                participants.get_index_of(from),
                participants.get_index_of(to),
            )
            && first != last
        {
            let centers = positions(&widths);
            let room = centers[first].abs_diff(centers[last]);
            let needed = visible_width(text) + usize::from(numbered) * 5 + 2;
            if room < needed {
                // Growing the preceding participant box creates room across both lifelines.
                widths[first.min(last)] += (needed - room) * 2;
            }
        }
    }
    let centers = positions(&widths);
    let total = widths.iter().sum::<usize>() + 2 * (widths.len() - 1);
    if total > width {
        return Ok(None);
    }
    let mut art = Art::default();
    sequence_boxes(&mut art, &participants, &widths, false);
    let mut number = 0;
    for step in steps {
        let mut stems = vec![' '; total];
        for &center in &centers {
            stems[center] = '│';
        }
        art.push(Role::Border, diagram_line(&stems, None));
        match step {
            SequenceStep::Divider(text) => {
                art.push(Role::Title, format!("── {text} ──"));
            }
            SequenceStep::Note(id, text) => {
                let x = participants
                    .get_index_of(id)
                    .map_or(0, |index| centers[index]);
                let note = format!("[ {text} ]");
                art.push(
                    Role::Text,
                    diagram_line(
                        &stems,
                        Some((x.saturating_sub(visible_width(&note) / 2), &note)),
                    ),
                );
            }
            SequenceStep::Message {
                from,
                to,
                text,
                dashed,
                headed,
            } => {
                let a = participants
                    .get_index_of(from)
                    .map_or(0, |index| centers[index]);
                let b = participants
                    .get_index_of(to)
                    .map_or(0, |index| centers[index]);
                number += 1;
                let text = if numbered {
                    Cow::Owned(format!("{number}. {text}"))
                } else {
                    Cow::Borrowed(text)
                };
                let start = a.min(b);
                let end = a.max(b);
                let label_at = if a == b {
                    a + 2
                } else {
                    start + (end - start).saturating_sub(visible_width(&text)) / 2
                };
                art.push(Role::Text, diagram_line(&stems, Some((label_at, &text))));
                let line = if dashed { '╌' } else { '─' };
                if a == b {
                    art.push(Role::Edge, diagram_line(&stems, Some((a, "├─┐"))));
                    art.push(Role::Edge, diagram_line(&stems, Some((a, "│◀┘"))));
                } else {
                    stems[start + 1..end].fill(line);
                    if a < b {
                        stems[start] = '├';
                        if headed {
                            stems[end - 1] = '▶';
                        }
                    } else {
                        stems[end] = '┤';
                        if headed {
                            stems[start + 1] = '◄';
                        }
                    }
                    art.push(Role::Edge, diagram_line(&stems, None));
                }
            }
        }
    }
    let cells = centers.last().copied().unwrap_or(0) + 1;
    let mut empty = vec![' '; cells];
    for center in centers {
        empty[center] = '│';
    }
    art.push(Role::Border, empty.into_iter().collect::<String>());
    sequence_boxes(&mut art, &participants, &widths, true);
    Ok(Some(art))
}

enum SequenceStep<'a> {
    Message {
        from: &'a str,
        to: &'a str,
        text: &'a str,
        dashed: bool,
        headed: bool,
    },
    Divider(&'a str),
    Note(&'a str, &'a str),
}

fn diagram_line(cells: &[char], label: Option<(usize, &str)>) -> String {
    if let Some((start, text)) = label {
        let end = start + visible_width(text);
        let mut line = cells[..start.min(cells.len())].iter().collect::<String>();
        line.extend(std::iter::repeat_n(' ', start.saturating_sub(cells.len())));
        line.push_str(text);
        line.extend(cells[end.min(cells.len())..].iter());
        line.truncate(line.trim_end().len());
        line
    } else {
        let count = cells
            .iter()
            .rposition(|&cell| cell != ' ')
            .map_or(0, |pos| pos + 1);
        cells[..count].iter().collect()
    }
}

fn sequence_boxes(
    art: &mut Art,
    participants: &IndexMap<&str, &str>,
    widths: &[usize],
    lower: bool,
) {
    let mut top = String::new();
    let mut middle = String::new();
    let mut bottom = String::new();
    for (&label, &width) in participants.values().zip(widths) {
        if !top.is_empty() {
            top.push_str("  ");
            middle.push_str("  ");
            bottom.push_str("  ");
        }
        let center = width / 2;
        top.push('┌');
        bottom.push('└');
        for cell in 1..width - 1 {
            top.push(if lower && cell == center {
                '┴'
            } else {
                '─'
            });
            bottom.push(if !lower && cell == center {
                '┬'
            } else {
                '─'
            });
        }
        top.push('┐');
        bottom.push('┘');
        let free = width - 2 - visible_width(label);
        middle.push('│');
        middle.extend(std::iter::repeat_n(' ', free / 2));
        middle.push_str(label);
        middle.extend(std::iter::repeat_n(' ', free - free / 2));
        middle.push('│');
    }
    art.push(Role::Border, top);
    art.push(Role::Text, middle);
    art.push(Role::Border, bottom);
}

pub(super) fn pie<'a>(
    header: &str,
    lines: impl Iterator<Item = &'a str>,
    width: usize,
) -> Result<Option<Art>, &'static str> {
    let title = header.split_once("title").map(|(_, title)| title.trim());
    let show_data = header.split_whitespace().any(|word| word == "showData");
    let mut slices = Vec::new();
    let mut titles = title;
    for line in lines.map(str::trim) {
        if let Some(title) = line.strip_prefix("title ") {
            titles = Some(title.trim());
            continue;
        }
        let (label, value) = line
            .rsplit_once(':')
            .ok_or("expected a labelled pie value")?;
        let value = value
            .trim()
            .parse::<f64>()
            .map_err(|_| "invalid pie value")?;
        if !value.is_finite() || value < 0.0 {
            return Err("invalid pie value");
        }
        slices.push((label.trim().trim_matches('"'), value));
    }
    let total: f64 = slices.iter().map(|(_, value)| value).sum();
    let label_width = slices
        .iter()
        .map(|(label, _)| visible_width(label))
        .max()
        .unwrap_or(0);
    if total <= 0.0 || !total.is_finite() {
        return Ok(None);
    }
    let bar_width = width.saturating_sub(label_width + 18).min(32);
    if bar_width == 0 {
        return Ok(None);
    }
    let mut art = Art::default();
    if let Some(title) = titles {
        art.push(Role::Title, title);
    }
    for (label, value) in slices {
        let fraction = value / total;
        let bar = (fraction * bar_width as f64).round() as usize;
        let label = format!("{label}{} ", " ".repeat(label_width - visible_width(label)));
        let graph = format!("{}{}", "█".repeat(bar), "░".repeat(bar_width - bar));
        let number = if show_data {
            format!(" {:>3.0}% ({value})", 100.0 * fraction)
        } else {
            format!(" {:>3.0}%", 100.0 * fraction)
        };
        art.0.push(vec![
            (Role::Text, label),
            (Role::Edge, graph),
            (Role::Text, number),
        ]);
    }
    Ok(Some(art))
}

pub(super) fn mindmap<'a>(
    lines: impl Iterator<Item = &'a str>,
) -> Result<Option<Art>, &'static str> {
    let mut map = Vec::new();
    let mut parents: Vec<usize> = Vec::new();
    for line in lines {
        let label = line.trim();
        let indent = line.len() - line.trim_start().len();
        if label.starts_with("::") {
            continue;
        }
        while parents.last().is_some_and(|&level| level >= indent) {
            parents.pop();
        }
        let level = parents.len();
        parents.push(indent);
        let label = label.find(['(', '[', '{']).map_or(label, |start| {
            label[start..].trim_matches(['(', ')', '[', ']', '{', '}', '"'])
        });
        if label.is_empty() {
            return Err("empty mindmap node");
        }
        map.push((level, label));
        if map.len() > 512 {
            return Ok(None);
        }
    }
    let mut art = Art::default();
    let mut guides = Vec::new();
    for (index, &(level, label)) in map.iter().enumerate() {
        if level == 0 {
            art.push(Role::Title, label);
            guides.clear();
            continue;
        }
        let later_sibling = map[index + 1..]
            .iter()
            .find(|(later_level, _)| *later_level <= level)
            .is_some_and(|(later_level, _)| *later_level == level);
        guides.truncate(level - 1);
        let prefix = guides.iter().copied().collect::<String>();
        let marker = if later_sibling {
            "├── "
        } else {
            "└── "
        };
        art.0.push(vec![
            (Role::Border, format!("{prefix}{marker}")),
            (Role::Text, label.to_owned()),
        ]);
        guides.push(if later_sibling { "│   " } else { "    " });
    }
    Ok((!art.0.is_empty()).then_some(art))
}

pub(super) fn timeline<'a>(
    lines: impl Iterator<Item = &'a str>,
) -> Result<Option<Art>, &'static str> {
    let mut art = Art::default();
    let mut period_seen = false;
    for line in lines.map(str::trim) {
        if let Some(title) = line.strip_prefix("title ") {
            art.push(Role::Title, title.trim());
        } else if let Some(section) = line.strip_prefix("section ") {
            art.push(Role::Title, format!("── {section} ──"));
        } else {
            let (period, events) = line
                .split_once(':')
                .ok_or("expected a timeline period and event")?;
            if !period.trim().is_empty() {
                art.push(Role::Edge, format!("● {}", period.trim()));
                period_seen = true;
            } else if !period_seen {
                return Err("timeline event has no period");
            }
            for event in events
                .split(':')
                .map(str::trim)
                .filter(|event| !event.is_empty())
            {
                art.0.push(vec![
                    (Role::Border, "  └─ ".to_owned()),
                    (Role::Text, event.to_owned()),
                ]);
            }
        }
    }
    Ok((!art.0.is_empty()).then_some(art))
}

pub(super) fn git_graph<'a>(
    lines: impl Iterator<Item = &'a str>,
) -> Result<Option<Art>, &'static str> {
    let mut branches = IndexMap::new();
    branches.insert("main", 0);
    let mut active = 0;
    let mut commits = Vec::new();
    for line in lines.map(str::trim) {
        let (command, args) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let args = args.trim();
        match command {
            "branch" if !args.is_empty() => {
                let branch = args.split_whitespace().next().unwrap_or(args);
                let index = branches.len();
                branches.entry(branch).or_insert(index);
            }
            "checkout" | "switch" => active = *branches.get(args).ok_or("unknown git branch")?,
            "commit" => commits.push((
                active,
                None,
                dequote(args.strip_prefix("id:").unwrap_or(args)).unwrap_or("commit"),
            )),
            "merge" => {
                let branch = args.split_whitespace().next().unwrap_or(args);
                let other = *branches.get(branch).ok_or("unknown merge branch")?;
                commits.push((active, Some(other), branch));
            }
            "cherry-pick" => commits.push((
                active,
                None,
                dequote(args.strip_prefix("id:").unwrap_or(args)).unwrap_or("cherry-pick"),
            )),
            _ => return Err("unsupported gitGraph command"),
        }
    }
    if commits.is_empty() || branches.len() > 32 {
        return Ok(None);
    }
    let mut art = Art::default();
    for (branch, merge, label) in commits.into_iter().rev() {
        let mut lanes = String::new();
        let mut names = String::new();
        for (index, (&name, _)) in branches.iter().enumerate() {
            let cell = if index == branch {
                names.push_str(name);
                if merge.is_some() { '◆' } else { '●' }
            } else if merge == Some(index) {
                '╯'
            } else if merge
                .is_some_and(|other| (branch.min(other)..branch.max(other)).contains(&index))
            {
                '─'
            } else {
                '│'
            };
            lanes.push(cell);
            lanes.push(
                if merge
                    .is_some_and(|other| (branch.min(other)..branch.max(other)).contains(&index))
                {
                    '─'
                } else {
                    ' '
                },
            );
        }
        art.0.push(vec![
            (Role::Edge, lanes),
            (Role::Title, format!("[{names}] ")),
            (Role::Text, label.to_owned()),
        ]);
    }
    Ok(Some(art))
}

fn dequote(text: &str) -> Option<&str> {
    let text = text.trim();
    if let Some(text) = text.strip_prefix('"') {
        text.split_once('"').map(|(text, _)| text)
    } else {
        text.split_whitespace().next()
    }
}
