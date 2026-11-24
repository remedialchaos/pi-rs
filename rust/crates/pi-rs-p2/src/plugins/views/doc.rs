//! Durable document and pure helpers for `views` (`p2.views`).

use std::{collections::HashSet, sync::Arc};

use pi_rs_chord::{Error, Result, context::Context, documents::DocumentKey};
use pi_rs_durable::{
    DocDefinition, DocToken, Session,
    documents::{DocumentSemantics, InitialDocument, resolve_address},
    session::Transaction,
};
use serde_json::{Map, Value, json};
use unicode_normalization::UnicodeNormalization;

use super::contract::{
    Annotation, AnnotationInput, MAX_ANNOTATIONS, SelectInlineInput, TRANSCRIPT, View, ViewsState,
    is_valid_view_id, live_views,
};

/// The session's views and annotations (`p2.views`, v1, session scope).
pub fn views_doc() -> DocToken {
    Arc::new(DocDefinition {
        kind: "p2.views".into(),
        version: 1,
        semantics: DocumentSemantics::Session,
        initial: InitialDocument::Singleton(Arc::new(|| {
            Ok(Map::from_iter([
                ("views".into(), json!([])),
                ("annotations".into(), json!([])),
            ]))
        })),
        migrate: None,
        checkpoint_when: Arc::new(|_, _, stats| stats.deltas_since_base >= 32),
    })
}

pub fn parse_views_state(obj: &Map<String, Value>) -> ViewsState {
    serde_json::from_value(Value::Object(obj.clone())).unwrap_or_default()
}

pub fn save_views_state(state: &ViewsState) -> Result<Map<String, Value>> {
    let Value::Object(map) = serde_json::to_value(state)? else {
        return Err(Error::Invalid("views state is not an object".into()));
    };
    Ok(map)
}

/// The session's live views and annotations as committed (empty before the first view).
pub async fn read_views(session: &Session, context: &Context) -> Result<ViewsState> {
    let doc = views_doc();
    let addr = resolve_address(&doc, None, None).map_err(|e| Error::Invalid(e.to_string()))?;
    let snapshot = session
        .snapshot(&doc, &addr, context)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let Some(obj) = snapshot else {
        return Ok(ViewsState::default());
    };
    let mut state = parse_views_state(&obj);
    state.views = live_views(&state);
    Ok(state)
}

/// Mutates the `p2.views` document inside a Durable transaction.
pub async fn update_views_doc<T>(
    tx: &mut Transaction<'_>,
    f: impl FnOnce(&mut ViewsState) -> Result<T>,
) -> Result<T> {
    let doc = views_doc();
    let addr = resolve_address(&doc, None, None).map_err(|e| Error::Invalid(e.to_string()))?;
    let draft = tx
        .doc(&doc, &addr, None)
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let current_val = draft
        .value()
        .map_err(|e| Error::Invalid(e.to_string()))?
        .to_json()
        .to_serde()?;
    let mut state: ViewsState = serde_json::from_value(current_val.clone()).unwrap_or_default();
    let out = f(&mut state)?;
    let next = save_views_state(&state)?;
    for (k, v) in &next {
        draft
            .set(
                &[pi_rs_chord::delta::Segment::Key(k.clone().into())],
                v.clone().into(),
            )
            .map_err(|e| Error::Invalid(e.to_string()))?;
    }
    if let Some(old_obj) = current_val.as_object() {
        for k in old_obj.keys() {
            if !next.contains_key(k) {
                draft
                    .delete(&[pi_rs_chord::delta::Segment::Key(k.clone().into())])
                    .map_err(|e| Error::Invalid(e.to_string()))?;
            }
        }
    }
    Ok(out)
}

pub fn with_key(_: Option<&DocumentKey>) -> Option<&DocumentKey> {
    None
}

/// A free view ID made from `title` (or `kind`): its slug, numbered when taken.
pub fn free_view_id(state: &ViewsState, title: &str, kind: &str) -> String {
    let normalized: String = title.to_lowercase().nfkd().collect();
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in normalized.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            slug.push(ch);
            prev_dash = false;
        } else if !prev_dash && !slug.is_empty() {
            slug.push('-');
            prev_dash = true;
        }
        if slug.len() >= 48 {
            break;
        }
    }
    let trimmed = slug.trim_matches('-');
    let slug = if trimmed.is_empty() { kind } else { trimmed };
    let base = if is_valid_view_id(slug) && slug != TRANSCRIPT {
        slug.to_string()
    } else {
        let prefixed = format!("view-{slug}");
        prefixed.chars().take(48).collect()
    };

    let taken: HashSet<&str> = state.views.iter().map(|v| v.id.as_str()).collect();
    if !taken.contains(base.as_str()) {
        return base;
    }
    for n in 2usize.. {
        let candidate = format!("{base}-{n}");
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
    }
    base
}

pub struct ViewSpec {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub item: Option<String>,
}

/// Opens a view: adds it, or updates the one with its ID (same kind; selection & unset fields stay)
/// and brings it to the front.
pub fn open_view(draft: &mut ViewsState, spec: ViewSpec, now: u64) -> Result<View> {
    let at = draft.views.iter().position(|each| each.id == spec.id);
    let existing = at.and_then(|idx| {
        let v = &draft.views[idx];
        (v.kind != "custom").then_some(v)
    });
    if let Some(ex) = existing
        && ex.kind != spec.kind
    {
        return Err(Error::Invalid(format!(
            "View \"{}\" is a {} view",
            spec.id, ex.kind
        )));
    }
    let view = View {
        id: spec.id,
        kind: spec.kind,
        item: spec
            .item
            .or_else(|| existing.and_then(|ex| ex.item.clone())),
        title: spec.title,
        selection: existing.and_then(|ex| ex.selection.clone()),
        focus: now,
        created: existing.map_or(now, |ex| ex.created),
    };
    match at {
        Some(idx) => draft.views[idx] = view.clone(),
        None => draft.views.push(view.clone()),
    }
    Ok(view)
}

/// Removes view `id` with its annotations; `false` when there is none.
pub fn remove_view(draft: &mut ViewsState, id: &str) -> bool {
    let Some(at) = draft.views.iter().position(|v| v.id == id) else {
        return false;
    };
    draft.views.remove(at);
    draft.annotations.retain(|a| a.view != id);
    true
}

fn to_base36(mut n: u64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(DIGITS[(n % 36) as usize] as char);
        n /= 36;
    }
    buf.into_iter().rev().collect()
}

/// Adds an annotation by `author` on a view of the session or the transcript; returns its ID.
pub fn add_annotation(
    draft: &mut ViewsState,
    input: AnnotationInput,
    author: &str,
    now: u64,
) -> Result<String> {
    let view_id = input.view();
    if view_id != TRANSCRIPT && !draft.views.iter().any(|v| v.id == view_id) {
        return Err(Error::Invalid(format!("No view \"{view_id}\"")));
    }
    let rand_part = (uuid::Uuid::new_v4().as_u128() % 1296) as u64;
    let id = format!("a{}{}", to_base36(now), to_base36(rand_part));
    let annotation = match input {
        AnnotationInput::Comment { view, quote, note } => Annotation {
            id: id.clone(),
            kind: "comment".into(),
            view,
            quote: Some(quote),
            note: Some(note),
            strokes: None,
            aspect: None,
            under: None,
            author: author.to_string(),
            created: now,
            sent: None,
        },
        AnnotationInput::Delete { view, quote } => Annotation {
            id: id.clone(),
            kind: "delete".into(),
            view,
            quote: Some(quote),
            note: None,
            strokes: None,
            aspect: None,
            under: None,
            author: author.to_string(),
            created: now,
            sent: None,
        },
        AnnotationInput::Ink {
            view,
            strokes,
            aspect,
            under,
            note,
        } => Annotation {
            id: id.clone(),
            kind: "ink".into(),
            view,
            quote: None,
            note,
            strokes: Some(strokes),
            aspect: Some(aspect),
            under,
            author: author.to_string(),
            created: now,
            sent: None,
        },
    };
    draft.annotations.push(annotation);
    if draft.annotations.len() > MAX_ANNOTATIONS {
        let excess = draft.annotations.len() - MAX_ANNOTATIONS;
        draft.annotations.drain(0..excess);
    }
    Ok(id)
}

/// Where a view is, as the agent reads it: `view "notes" (markdown docs/notes.md, "Notes")`.
pub fn view_name(views: &[View], id: &str) -> String {
    if id == TRANSCRIPT {
        return "the transcript".to_string();
    }
    let Some(view) = views.iter().find(|v| v.id == id) else {
        return format!("view \"{id}\" (closed)");
    };
    let where_str = match &view.item {
        Some(item) if !item.is_empty() => format!(" {item}"),
        _ => String::new(),
    };
    format!(
        "view \"{id}\" ({}{where_str}, \"{}\")",
        view.kind, view.title
    )
}

/// Text quoted on one line, shortened to `max` characters.
pub fn quoted(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let utf16_len: usize = line.chars().map(char::len_utf16).sum();
    if utf16_len <= max {
        return format!("«{line}»");
    }
    let mut units = 0usize;
    let mut end = 0usize;
    for (idx, ch) in line.char_indices() {
        let u = ch.len_utf16();
        if units + u > max {
            break;
        }
        units += u;
        end = idx + ch.len_utf8();
    }
    format!("«{}…»", &line[..end])
}

fn ink_box(strokes: &[Vec<f64>]) -> String {
    let mut left = 1.0_f64;
    let mut top = 1.0_f64;
    let mut right = 0.0_f64;
    let mut bottom = 0.0_f64;
    for stroke in strokes {
        let mut i = 0;
        while i + 1 < stroke.len() {
            let x = stroke[i];
            let y = stroke[i + 1];
            left = left.min(x);
            right = right.max(x);
            top = top.min(y);
            bottom = bottom.max(y);
            i += 2;
        }
    }
    if right < left {
        return String::new();
    }
    let pct = |v: f64| (v.clamp(0.0, 1.0) * 100.0).round() as i64;
    format!(
        " (strokes at {}–{}% across, {}–{}% down the pane)",
        pct(left),
        pct(right),
        pct(top),
        pct(bottom)
    )
}

/// One annotation as the agent reads it.
pub fn describe_annotation(views: &[View], annotation: &Annotation) -> String {
    let where_str = view_name(views, &annotation.view);
    let sent_note = if annotation.sent == Some(true) {
        ", sent before"
    } else {
        ""
    };
    let by = format!(" ({}{sent_note}, id {})", annotation.author, annotation.id);
    match annotation.kind.as_str() {
        "comment" => format!(
            "Comment in {where_str} on {}: {}{by}",
            quoted(annotation.quote.as_deref().unwrap_or(""), 600),
            annotation.note.as_deref().unwrap_or("")
        ),
        "delete" => format!(
            "Remove from {where_str}: {}{by}",
            quoted(annotation.quote.as_deref().unwrap_or(""), 600)
        ),
        "ink" => {
            let under = annotation
                .under
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|u| format!(" over {}", quoted(u, 600)))
                .unwrap_or_default();
            let note = annotation
                .note
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|n| format!(": {n}"))
                .unwrap_or_default();
            let box_str = ink_box(annotation.strokes.as_deref().unwrap_or(&[]));
            format!("Drawing in {where_str}{under}{box_str}{note}{by}")
        }
        _ => format!("Annotation in {where_str}{by}"),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CarriedSelection {
    pub view: String,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CarriedInline {
    pub key: String,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Carried {
    pub text: String,
    pub annotations: Vec<String>,
    pub selections: Vec<CarriedSelection>,
    pub inline: Vec<CarriedInline>,
}

pub fn inline_key(kind: &str, item: &str) -> String {
    format!("{kind}:{item}")
}

/// What a person's next message carries to the agent: unsent selections and annotations.
pub fn view_context(state: &ViewsState) -> Option<Carried> {
    let selections: Vec<&View> = state
        .views
        .iter()
        .filter(|v| v.selection.as_ref().is_some_and(|s| s.sent != Some(true)))
        .collect();
    let inline: Vec<&super::contract::InlineSelection> = state
        .inline
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter(|each| each.selection.sent != Some(true))
        .collect();
    let annotations: Vec<&Annotation> = state
        .annotations
        .iter()
        .filter(|a| a.sent != Some(true))
        .collect();

    if selections.is_empty() && inline.is_empty() && annotations.is_empty() {
        return None;
    }

    let mut lines = vec![
        "<views>".to_string(),
        "From the views beside the transcript (code mode's view_* tools list and change them):"
            .to_string(),
    ];
    for view in &selections {
        let summary = view.selection.as_ref().map_or("", |s| s.summary.as_str());
        lines.push(format!(
            "Selected in {}: {summary}",
            view_name(&state.views, &view.id)
        ));
    }
    for each in &inline {
        lines.push(format!(
            "Selected in {} \"{}\" (\"{}\") shown in the transcript: {}",
            each.kind, each.item, each.title, each.selection.summary
        ));
    }
    if !annotations.is_empty() {
        lines.push("Annotations the person made:".to_string());
        for (idx, annotation) in annotations.iter().enumerate() {
            lines.push(format!(
                "{}. {}",
                idx + 1,
                describe_annotation(&state.views, annotation)
            ));
        }
    }
    lines.push("</views>".to_string());

    Some(Carried {
        text: lines.join("\n"),
        annotations: annotations.iter().map(|a| a.id.clone()).collect(),
        selections: selections
            .iter()
            .map(|v| CarriedSelection {
                view: v.id.clone(),
                summary: v
                    .selection
                    .as_ref()
                    .map_or_else(String::new, |s| s.summary.clone()),
            })
            .collect(),
        inline: inline
            .iter()
            .map(|each| CarriedInline {
                key: inline_key(&each.kind, &each.item),
                summary: each.selection.summary.clone(),
            })
            .collect(),
    })
}

/// Marks what a message carried as sent: those annotations, and those selections unless they changed meanwhile.
pub fn mark_sent(draft: &mut ViewsState, carried: &Carried) {
    let ann_set: HashSet<&str> = carried.annotations.iter().map(String::as_str).collect();
    for annotation in &mut draft.annotations {
        if ann_set.contains(annotation.id.as_str()) {
            annotation.sent = Some(true);
        }
    }
    for CarriedSelection { view, summary } in &carried.selections {
        if let Some(v) = draft.views.iter_mut().find(|each| &each.id == view)
            && let Some(sel) = &mut v.selection
            && &sel.summary == summary
        {
            sel.sent = Some(true);
        }
    }
    if let Some(inline_list) = &mut draft.inline {
        for CarriedInline { key, summary } in &carried.inline {
            if let Some(each) = inline_list
                .iter_mut()
                .find(|each| &inline_key(&each.kind, &each.item) == key)
                && &each.selection.summary == summary
            {
                each.selection.sent = Some(true);
            }
        }
    }
}

/// Sets (or with `None` clears) the person's selection in an item shown in the transcript.
pub fn select_inline(draft: &mut ViewsState, input: SelectInlineInput) {
    let key = inline_key(&input.kind, &input.item);
    let mut others: Vec<super::contract::InlineSelection> = draft
        .inline
        .take()
        .unwrap_or_default()
        .into_iter()
        .filter(|each| inline_key(&each.kind, &each.item) != key)
        .collect();
    if let Some(sel) = input.selection {
        others.push(super::contract::InlineSelection {
            kind: input.kind,
            item: input.item,
            title: input.title,
            selection: super::contract::Selection {
                summary: sel.summary,
                data: sel.data,
                sent: None,
            },
        });
    }
    draft.inline = Some(others);
}
