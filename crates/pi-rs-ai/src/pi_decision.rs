use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::classifier::{
    ClassifierOptions, classifier_failed, classifier_output, post_classifier_request,
};
use crate::error::{Error, Result};
use crate::http::endpoint;
use crate::model::{ClassifierApi, ClassifierModel, ModelInputModality};
use crate::request::ProviderHeaders;
use crate::types::{
    ClassifierAnswer, ClassifierContext, ClassifierQuestion, ClassifierResult, Cost, Usage,
};

const LABEL: &str = "Pi Decision";

pub async fn classify(
    model: &ClassifierModel,
    context: &ClassifierContext,
    options: &ClassifierOptions,
) -> ClassifierResult {
    let mut output = classifier_output(model);
    if let Err(error) = run(&mut output, model, context, options).await {
        output.answers.clear();
        classifier_failed(&mut output, error, &options.request);
    }
    output
}

fn finite_non_negative(value: &Value) -> f64 {
    value
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .unwrap_or(0.0)
}

fn required_number(value: &Value, field: &str) -> Result<f64> {
    value
        .as_f64()
        .filter(|n| n.is_finite())
        .ok_or_else(|| Error::Protocol(format!("{LABEL} returned an invalid {field}")))
}

fn required_unit_interval(value: &Value, field: &str) -> Result<f64> {
    let num = required_number(value, field)?;
    if !(0.0..=1.0).contains(&num) {
        return Err(Error::Protocol(format!(
            "{LABEL} returned out-of-range {field}: {num}"
        )));
    }
    Ok(num)
}

fn parse_usage(value: &Value, model: &ClassifierModel) -> Option<Usage> {
    let obj = value.as_object()?;
    if !obj.get("input").is_some_and(Value::is_number)
        && !obj.get("output").is_some_and(Value::is_number)
    {
        return None;
    }
    let input = finite_non_negative(&value["input"]);
    let output = finite_non_negative(&value["output"]);
    let cache_read = finite_non_negative(&value["cacheRead"]);
    let cache_write = finite_non_negative(&value["cacheWrite"]);
    let total_tokens = value["totalTokens"]
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .unwrap_or(input + output + cache_read + cache_write);
    let mut usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        total_tokens,
        ..Usage::default()
    };
    let cost = &value["cost"];
    if cost.is_object()
        && cost["total"]
            .as_f64()
            .is_some_and(|n| n.is_finite() && n >= 0.0)
    {
        usage.cost = Cost {
            input: finite_non_negative(&cost["input"]),
            output: finite_non_negative(&cost["output"]),
            cache_read: finite_non_negative(&cost["cacheRead"]),
            cache_write: finite_non_negative(&cost["cacheWrite"]),
            total: finite_non_negative(&cost["total"]),
            total_from_provider: cost["totalFromProvider"].as_bool(),
        };
    } else {
        usage.calculate_cost(&model.cost);
    }
    Some(usage)
}

fn parse_probabilities(
    value: &Value,
    id: &str,
    criteria: &IndexMap<String, String>,
) -> Result<IndexMap<String, f64>> {
    let obj = value.as_object().ok_or_else(|| {
        Error::Protocol(format!("{LABEL} returned invalid probabilities for {id}"))
    })?;
    obj.iter()
        .map(|(choice, prob)| {
            if !criteria.contains_key(choice) {
                return Err(Error::Protocol(format!(
                    "{LABEL} returned unknown choice \"{choice}\" in probabilities for {id}"
                )));
            }
            let p = required_unit_interval(prob, &format!("probability for {id}.{choice}"))?;
            Ok((choice.clone(), p))
        })
        .collect()
}

fn parse_answer(
    id: &str,
    question: &ClassifierQuestion,
    answer: &Value,
) -> Result<ClassifierAnswer> {
    if !answer.is_object() {
        return Err(Error::Protocol(format!(
            "{LABEL} returned an invalid answer for {id}"
        )));
    }
    let kind = answer["type"].as_str();
    match question {
        ClassifierQuestion::Choice { criteria, .. } => {
            let choice = answer["choice"]
                .as_str()
                .filter(|_| kind == Some("choice"))
                .ok_or_else(|| {
                    Error::Protocol(format!("{LABEL} did not return a choice answer for {id}"))
                })?;
            if !criteria.contains_key(choice) {
                return Err(Error::Protocol(format!(
                    "{LABEL} returned unknown choice \"{choice}\" for {id}"
                )));
            }
            Ok(ClassifierAnswer::Choice {
                choice: choice.to_owned(),
                probabilities: parse_probabilities(&answer["probabilities"], id, criteria)?,
                confidence: required_unit_interval(
                    &answer["confidence"],
                    &format!("confidence for {id}"),
                )?,
            })
        }
        ClassifierQuestion::Score { criteria, .. } => {
            if kind != Some("score") {
                return Err(Error::Protocol(format!(
                    "{LABEL} did not return a score answer for {id}"
                )));
            }
            let score = required_number(&answer["score"], &format!("score for {id}"))?;
            let max_score = criteria.len().saturating_sub(1) as f64;
            if !(0.0..=max_score).contains(&score) {
                return Err(Error::Protocol(format!(
                    "{LABEL} returned out-of-range score for {id}: {score}"
                )));
            }
            Ok(ClassifierAnswer::Score {
                score,
                confidence: required_unit_interval(
                    &answer["confidence"],
                    &format!("confidence for {id}"),
                )?,
            })
        }
        ClassifierQuestion::Bool { .. } => {
            if kind != Some("bool") {
                return Err(Error::Protocol(format!(
                    "{LABEL} did not return a bool answer for {id}"
                )));
            }
            Ok(ClassifierAnswer::Bool {
                probability: required_unit_interval(
                    &answer["probability"],
                    &format!("probability for {id}"),
                )?,
            })
        }
    }
}

fn parse_answers(
    value: &Value,
    questions: &IndexMap<String, ClassifierQuestion>,
) -> Result<IndexMap<String, ClassifierAnswer>> {
    let obj = value
        .as_object()
        .ok_or_else(|| Error::Protocol(format!("{LABEL} returned an unexpected response")))?;
    questions
        .iter()
        .map(|(id, question)| {
            let answer = obj.get(id).ok_or_else(|| {
                Error::Protocol(format!("{LABEL} did not return an answer for {id}"))
            })?;
            Ok((id.clone(), parse_answer(id, question, answer)?))
        })
        .collect()
}

async fn run(
    output: &mut ClassifierResult,
    model: &ClassifierModel,
    context: &ClassifierContext,
    options: &ClassifierOptions,
) -> Result<()> {
    if model.api != ClassifierApi::PiDecision {
        return Err(Error::Protocol(format!(
            "Unsupported classifier API: {}",
            model.api
        )));
    }
    if !context.images.is_empty() && !model.input.contains(&ModelInputModality::Image) {
        return Err(Error::Protocol(format!(
            "{LABEL} model {} does not support image input",
            model.id
        )));
    }
    let url = endpoint(&model.base_url, "decision")?;
    let mut request_body = json!({
        "model": model.id,
        "context": context,
    });
    if let Some(session_id) = options.session_id.as_deref() {
        request_body["options"] = json!({ "sessionId": session_id });
    }
    let body = post_classifier_request(
        model,
        &options.request,
        url,
        request_body,
        LABEL,
        ProviderHeaders::new(),
        &[],
    )
    .await?;
    if !body.is_object() {
        return Err(Error::Protocol(format!(
            "{LABEL} returned an unexpected response"
        )));
    }
    output.response_id = body["responseId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if let Some(usage) = parse_usage(&body["usage"], model) {
        output.usage = Some(usage);
    }
    output.answers = parse_answers(&body["answers"], &context.questions)?;
    Ok(())
}
