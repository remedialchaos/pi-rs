use pi_rs_ai::model::ModelInputModality;
use pi_rs_chord::{Error, Result, context::Context};
use pi_rs_durable::{
    harness::Conversation,
    images::{FittedImage, ImageLimits, ImageProcessor, fit_image, image_limits},
};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::LazyLock;

use crate::api::v1::worker::WorkerProcess;

use super::conversation::{invalid, text};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct File {
    pub id: String,
    pub name: String,
    path: String,
    local_path: Option<String>,
    mime_type: Option<String>,
}

pub(super) fn files(args: &Value) -> Result<Vec<File>> {
    args.get("files").map_or_else(
        || Ok(Vec::new()),
        |files| Ok(serde_json::from_value(files.clone())?),
    )
}

static REFERENCES: LazyLock<std::result::Result<Regex, regex::Error>> =
    LazyLock::new(|| Regex::new(r"!?\[(?:\\.|[^\\\]\n])*\]\(attachment:([A-Za-z0-9_-]+)\)"));

pub(super) async fn content(
    message: &str,
    author: &Value,
    files: &[File],
    conversation: &Conversation,
    worker: &WorkerProcess,
    cap: Option<u64>,
    context: &Context,
) -> Result<Value> {
    let name = escape(text(author, "name")?);
    let sender = if author["kind"] == "owner" {
        format!("<sender kind=\"owner\" name=\"{name}\"/>")
    } else {
        format!(
            "<sender kind=\"guest\" id=\"{}\" name=\"{name}\"/>",
            escape(text(author, "id")?)
        )
    };
    let mut blocks = vec![json!({"type":"text","text":sender})];
    if files.is_empty() {
        blocks.push(json!({"type":"text","text":message}));
        return Ok(json!(blocks));
    }

    let agent = conversation.agent(context).await.map_err(invalid)?;
    let models = worker.harness()?.durable.models();
    let model = agent.state["model"]["provider"]
        .as_str()
        .zip(agent.state["model"]["modelId"].as_str())
        .and_then(|(provider, id)| models.get_model(provider, id));
    let accepts = model
        .as_ref()
        .is_some_and(|model| model.input.contains(&ModelInputModality::Image));
    let limit = cap
        .and_then(|limit| usize::try_from(limit).ok())
        .unwrap_or(usize::MAX);
    let inline_max = if accepts { limit } else { 0 };
    let references = REFERENCES.as_ref().map_err(invalid)?;
    let mut ordered = Vec::new();
    let refs = references
        .captures_iter(message)
        .filter_map(|capture| {
            let span = capture.get(0)?;
            let attachment = capture.get(1)?.as_str();
            let index = files.iter().position(|file| file.id == attachment)?;
            if !ordered.contains(&index) {
                ordered.push(index);
            }
            Some((span.start(), span.end(), index))
        })
        .collect::<Vec<_>>();
    for index in 0..files.len() {
        if !ordered.contains(&index) {
            ordered.push(index);
        }
    }
    let inline = ordered
        .iter()
        .rev()
        .copied()
        .filter(|&index| files[index].mime_type.is_some())
        .take(inline_max)
        .collect::<Vec<_>>();
    let mut names = vec![String::new(); files.len()];
    let (mut image_number, mut file_number) = (0, 0);
    for &index in &ordered {
        names[index] = if files[index].mime_type.is_some() {
            image_number += 1;
            format!("[Image #{image_number}]")
        } else {
            file_number += 1;
            format!("[File #{file_number}]")
        };
    }
    let mut output = Content {
        blocks,
        pending: String::new(),
        placed: vec![false; files.len()],
        accepts,
        inline,
        names,
        files,
        limits: image_limits(model.as_ref()),
        processor: &worker.images,
    };
    let mut offset = 0;
    for (start, end, index) in refs {
        let preceding = message
            .get(offset..start)
            .ok_or_else(|| Error::Invalid("Invalid reference offset".into()))?;
        output.pending.push_str(preceding);
        output.place(index).await?;
        offset = end;
    }
    let trailing = message
        .get(offset..)
        .ok_or_else(|| Error::Invalid("Invalid reference offset".into()))?;
    output.pending.push_str(trailing);
    let extras = ordered
        .into_iter()
        .filter(|&index| !output.placed[index])
        .collect::<Vec<_>>();
    for (position, index) in extras.into_iter().enumerate() {
        if position > 0 {
            output.pending.push('\n');
        } else if !output.pending.trim().is_empty() {
            output.pending.push_str("\n\n");
        }
        output.place(index).await?;
    }
    if !output.pending.is_empty() {
        output
            .blocks
            .push(json!({"type":"text","text":output.pending}));
    }
    Ok(json!(output.blocks))
}

struct Content<'a> {
    blocks: Vec<Value>,
    pending: String,
    placed: Vec<bool>,
    accepts: bool,
    inline: Vec<usize>,
    names: Vec<String>,
    files: &'a [File],
    limits: ImageLimits,
    processor: &'a dyn ImageProcessor,
}

impl Content<'_> {
    async fn place(&mut self, index: usize) -> Result<()> {
        use std::fmt::Write;
        let display = &self.names[index];
        if self.placed[index] {
            self.pending.push_str(display);
            return Ok(());
        }
        self.placed[index] = true;
        let file = &self.files[index];
        let Some(mime) = &file.mime_type else {
            write!(
                self.pending,
                "<file name={display} path=\"{}\" />",
                file.path
            )
            .map_err(invalid)?;
            return Ok(());
        };
        write!(
            self.pending,
            "<image name={display} path=\"{}\">",
            file.path
        )
        .map_err(invalid)?;
        if !self.inline.contains(&index) {
            if self.accepts {
                self.pending
                    .push_str("(not shown: over this message's image limit)");
            }
        } else {
            let bytes = tokio::fs::read(file.local_path.as_deref().unwrap_or(&file.path)).await?;
            let note = match fit_image(bytes, mime, self.limits, self.processor).await {
                FittedImage::Fitted { image, notes } => {
                    self.blocks
                        .push(json!({"type":"text","text":std::mem::take(&mut self.pending)}));
                    let mut image = serde_json::to_value(image)?;
                    image["type"] = json!("image");
                    self.blocks.push(image);
                    notes.join(" ")
                }
                FittedImage::Omitted { note } => note,
            };
            self.pending.push_str(&note);
        }
        self.pending.push_str("</image>");
        Ok(())
    }
}

pub(super) fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
