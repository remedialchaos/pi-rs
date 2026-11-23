//! Context expressions from the upstream TUI command registry.
#[derive(Default, Clone)]
pub(super) enum When {
    #[default]
    Always,
    Term(String),
    Not(Box<When>),
    And(Vec<When>),
    Or(Vec<When>),
}
impl When {
    pub(super) fn parse(source: &str) -> Option<Self> {
        let mut source = source.trim();
        if source.is_empty() {
            return Some(Self::Always);
        }
        let expression = parse_or(&mut source, 0)?;
        source.trim().is_empty().then_some(expression)
    }
    pub(super) fn matches(&self, active: &[&str]) -> bool {
        match self {
            Self::Always => true,
            Self::Term(term) => active.contains(&term.as_str()),
            Self::Not(e) => !e.matches(active),
            Self::And(list) => list.iter().all(|e| e.matches(active)),
            Self::Or(list) => list.iter().any(|e| e.matches(active)),
        }
    }
    pub(super) fn specificity(&self) -> usize {
        match self {
            Self::Always => 0,
            Self::Term(_) => 1,
            Self::Not(e) => e.specificity(),
            Self::And(list) | Self::Or(list) => list.iter().map(Self::specificity).sum(),
        }
    }
}
fn take(source: &mut &str, token: &str) -> bool {
    if let Some(rest) = source.trim_start().strip_prefix(token) {
        *source = rest;
        true
    } else {
        false
    }
}
fn parse_or(source: &mut &str, depth: usize) -> Option<When> {
    let first = parse_and(source, depth)?;
    if !take(source, "||") {
        return Some(first);
    }
    let mut list = vec![first];
    loop {
        list.push(parse_and(source, depth)?);
        if !take(source, "||") {
            return Some(When::Or(list));
        }
    }
}
fn parse_and(source: &mut &str, depth: usize) -> Option<When> {
    let first = parse_unary(source, depth)?;
    if !take(source, "&&") {
        return Some(first);
    }
    let mut list = vec![first];
    loop {
        list.push(parse_unary(source, depth)?);
        if !take(source, "&&") {
            return Some(When::And(list));
        }
    }
}
fn parse_unary(source: &mut &str, depth: usize) -> Option<When> {
    if depth > 64 {
        return None;
    }
    if take(source, "!") {
        return Some(When::Not(Box::new(parse_unary(source, depth + 1)?)));
    }
    if take(source, "(") {
        let inner = parse_or(source, depth + 1)?;
        return take(source, ")").then_some(inner);
    }
    let text = source.trim_start();
    if !text.starts_with(|ch: char| ch.is_ascii_alphabetic()) {
        return None;
    }
    let end = text
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || "_:.-".contains(ch)))
        .unwrap_or(text.len());
    let (term, rest) = text.split_at(end);
    *source = rest;
    Some(When::Term(term.into()))
}
