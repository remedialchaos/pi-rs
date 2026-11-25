use indexmap::IndexMap;
use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::classifier::{
    ClassifierOptions, classifier_failed, classifier_output, parse_classifier_usage,
    post_classifier_request,
};
use crate::error::{Error, Result};
use crate::http::endpoint;
use crate::model::{ClassifierApi, ClassifierModel};
use crate::openrouter_images::image_data_url;
use crate::types::{ClassifierAnswer, ClassifierContext, ClassifierQuestion, ClassifierResult};

const LABEL: &str = "OpenAI Decisions";

pub async fn classify(
    model: &ClassifierModel,
    context: &ClassifierContext,
    options: &ClassifierOptions,
) -> ClassifierResult {
    let mut out = classifier_output(model);
    if let Err(error) = run(&mut out, model, context, options).await {
        let error = if matches!(
            &error,
            Error::Http {
                status: StatusCode::GATEWAY_TIMEOUT,
                ..
            }
        ) {
            Error::Protocol(format!(
                "{LABEL} error (504): the request timed out at the gateway. Very large inputs (above roughly 600K tokens) currently exceed its time limit."
            ))
        } else {
            error
        };
        classifier_failed(&mut out, error, &options.request);
    }
    out
}

fn wire_question(name: &str, question: &ClassifierQuestion) -> Value {
    match question {
        ClassifierQuestion::Choice {
            instructions,
            criteria,
        } => {
            let choices: Vec<_> = criteria
                .iter()
                .map(|(value, description)| {
                    if description.is_empty() {
                        json!({"value":value})
                    } else {
                        json!({"value":value,"description":description})
                    }
                })
                .collect();
            json!({"type":"choice","name":name,"instructions":instructions,"choices":choices})
        }
        ClassifierQuestion::Score {
            instructions,
            criteria,
        } => {
            json!({"type":"score","name":name,"instructions":instructions,"levels":criteria.iter().map(|label|json!({"label":label})).collect::<Vec<_>>() })
        }
        ClassifierQuestion::Bool {
            instructions,
            criteria,
        } => {
            let mut instructions = instructions.clone();
            if !criteria.yes.is_empty() || !criteria.no.is_empty() {
                instructions.push_str("\n\n");
            }
            if !criteria.yes.is_empty() {
                instructions.push_str("True means: ");
                instructions.push_str(&criteria.yes);
            }
            if !criteria.no.is_empty() {
                if !criteria.yes.is_empty() {
                    instructions.push('\n');
                }
                instructions.push_str("False means: ");
                instructions.push_str(&criteria.no);
            }
            json!({"type":"predicate","name":name,"instructions":instructions})
        }
    }
}

async fn run(
    out: &mut ClassifierResult,
    model: &ClassifierModel,
    context: &ClassifierContext,
    options: &ClassifierOptions,
) -> Result<()> {
    if model.api != ClassifierApi::OpenAiDecisions {
        return Err(Error::Protocol(format!(
            "Unsupported classifier API: {}",
            model.api
        )));
    }
    let state = serde_json::to_string(&context.state)?;
    let input = if context.images.is_empty() {
        Value::String(state)
    } else {
        if context.images.len() > 128 {
            return Err(Error::Protocol(format!(
                "{LABEL} accepts at most 128 images, got {}",
                context.images.len()
            )));
        }
        let mut parts = Vec::with_capacity(1 + context.images.len());
        parts.push(json!({"type":"input_text","text":state}));
        parts.extend(
            context
                .images
                .iter()
                .map(|image| json!({"type":"input_image","image_url":image_data_url(image)})),
        );
        json!([{"role":"user","content":parts}])
    };
    let questions = context
        .questions
        .iter()
        .map(|(name, q)| wire_question(name, q))
        .collect::<Vec<_>>();
    let body = post_classifier_request(
        model,
        &options.request,
        endpoint(&model.base_url, "decisions")?,
        json!({"model":model.id,"input":input,"questions":questions}),
        LABEL,
        Default::default(),
        &[StatusCode::GATEWAY_TIMEOUT],
    )
    .await?;
    if !body.is_object() {
        return Err(Error::Protocol(format!(
            "{LABEL} returned an unexpected response"
        )));
    }
    out.usage = parse_classifier_usage(&body["usage"], model);
    let answers = body["answers"]
        .as_array()
        .ok_or_else(|| Error::Protocol(format!("{LABEL} returned an unexpected response")))?;
    let by_name: IndexMap<_, _> = answers
        .iter()
        .filter_map(|a| Some((a["name"].as_str()?, a)))
        .collect();
    let mut parsed = IndexMap::with_capacity(context.questions.len());
    for (id, question) in &context.questions {
        let a = by_name
            .get(id.as_str())
            .ok_or_else(|| Error::Protocol(format!("{LABEL} did not return an answer for {id}")))?;
        let kind = a["type"].as_str();
        let wrong =
            |want| Error::Protocol(format!("{LABEL} did not return a {want} answer for {id}"));
        let number = |key| {
            a[key].as_f64().ok_or_else(|| {
                Error::Protocol(format!("{LABEL} returned an invalid {key} for {id}"))
            })
        };
        if kind == Some("refusal") {
            return Err(Error::Protocol(format!("{LABEL} refused to answer {id}")));
        }
        let answer = match question {
            ClassifierQuestion::Choice { .. } => {
                let choice = a["choice"]
                    .as_str()
                    .filter(|_| kind == Some("choice"))
                    .ok_or_else(|| wrong("choice"))?;
                let invalid =
                    || Error::Protocol(format!("{LABEL} returned invalid probabilities for {id}"));
                let probabilities = a["probabilities"]
                    .as_array()
                    .ok_or_else(invalid)?
                    .iter()
                    .map(|p| {
                        let value = p["value"].as_str().ok_or_else(invalid)?;
                        let prob = p["probability"].as_f64().ok_or_else(|| {
                            Error::Protocol(format!(
                                "{LABEL} returned an invalid probability for {id}.{value}"
                            ))
                        })?;
                        Ok((value.to_owned(), prob))
                    })
                    .collect::<Result<_>>()?;
                ClassifierAnswer::Choice {
                    choice: choice.to_owned(),
                    probabilities,
                    confidence: number("confidence")?,
                }
            }
            ClassifierQuestion::Score { .. } if kind == Some("score") => ClassifierAnswer::Score {
                score: number("score")?,
                confidence: number("confidence")?,
            },
            ClassifierQuestion::Bool { .. } if kind == Some("predicate") => {
                ClassifierAnswer::Bool {
                    probability: number("probability")?,
                }
            }
            ClassifierQuestion::Score { .. } => return Err(wrong("score")),
            ClassifierQuestion::Bool { .. } => return Err(wrong("predicate")),
        };
        parsed.insert(id.clone(), answer);
    }
    out.answers = parsed;
    Ok(())
}
