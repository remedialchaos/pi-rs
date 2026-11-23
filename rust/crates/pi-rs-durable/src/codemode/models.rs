//! Script-facing model catalog and keyed model tools.

use std::fmt;

use async_trait::async_trait;
use pi_rs_ai::{
    AnyModel, AssistantImages, ClassifierContext, ClassifierModel, ClassifierResult, ImageModel,
    ImagesContext, InputContent, Model, ModelType, Models, ProviderRequestOptions, StopReason,
    Usage, classifier::ClassifierOptions,
};
use pi_rs_chord::context::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{CodemodeError, CodemodeNestedResult, CodemodeToolSpec};

pub const CODEMODE_INTERNAL_PREFIX: &str = "pi.codemode.";
pub const CLASSIFY_TOOL: &str = "pi.codemode.models.classify";
pub const GENERATE_IMAGES_TOOL: &str = "pi.codemode.models.generateImages";
pub const MAX_CONCURRENT_MODEL_CALLS: usize = 4;
pub(super) const CATALOG_GLOBALS: [&str; 3] = [
    "models.getModelsOfType",
    "models.getAvailableOfType",
    "models.getModelOfType",
];
pub(super) const MODEL_CALLS: [(&str, &str); 2] = [
    ("models.classify", CLASSIFY_TOOL),
    ("models.generateImages", GENERATE_IMAGES_TOOL),
];

pub const MODELS_DESCRIPTION: &str = "`models` (all functions return promises):\n\
- `models.getModelsOfType(type, provider?)`, `models.getAvailableOfType(type, provider?)` (configured auth only), `models.getModelOfType(type, provider, id)`: the catalog; `type` is \"chat\", \"image\", or \"classifier\".\n\
- `models.classify(model, { state: { ... }, images?: [{ type: \"image\", data, mimeType }], questions: { <id>: { type: \"choice\", instructions, criteria: { <label>: <meaning> } } | { type: \"score\", instructions, criteria: [<lowest level>, ..., <highest level>] } | { type: \"bool\", instructions, criteria: { true: <meaning>, false: <meaning> } } } })` resolves to `{ answers: { <id>: ... }, stopReason, errorMessage?, usage? }`.\n\
- `models.generateImages(model, { input: [{ type: \"text\", text: <prompt> }, ...optional { type: \"image\", data, mimeType }] })` resolves to `{ output: [image blocks], stopReason, errorMessage?, usage? }`; show each image with `image(block)`.\n\
At most 4 classify/generateImages calls run at once; more wait.";

#[async_trait]
pub trait CodemodeModelProvider: Send + Sync {
    fn models_of_type(&self, kind: ModelType, provider: Option<&str>) -> Vec<AnyModel>;
    fn model_of_type(&self, kind: ModelType, provider: &str, id: &str) -> Option<AnyModel>;
    async fn available_of_type(
        &self,
        kind: ModelType,
        provider: Option<&str>,
        context: &Context,
    ) -> Result<Vec<AnyModel>, CodemodeError>;
    async fn classify(
        &self,
        model: ClassifierModel,
        input: &ClassifierContext,
        context: &Context,
    ) -> ClassifierResult;
    async fn generate_images(
        &self,
        model: ImageModel,
        input: &ImagesContext,
        context: &Context,
    ) -> AssistantImages;
}

#[async_trait]
impl CodemodeModelProvider for Models {
    fn models_of_type(&self, kind: ModelType, provider: Option<&str>) -> Vec<AnyModel> {
        self.get_models_of_type(kind, provider)
    }

    fn model_of_type(&self, kind: ModelType, provider: &str, id: &str) -> Option<AnyModel> {
        self.get_model_of_type(kind, provider, id)
    }

    async fn available_of_type(
        &self,
        kind: ModelType,
        provider: Option<&str>,
        context: &Context,
    ) -> Result<Vec<AnyModel>, CodemodeError> {
        let options = ProviderRequestOptions::<Model>::default();
        let _abort = context.abort_signal().map(|signal| {
            let token = options.signal.clone();
            signal.subscribe(move |_| token.cancel())
        });
        let mut models = self.get_all_available(provider, &options.signal).await?;
        models.retain(|model| model.model_type() == kind);
        Ok(models)
    }

    async fn classify(
        &self,
        model: ClassifierModel,
        input: &ClassifierContext,
        context: &Context,
    ) -> ClassifierResult {
        let options = ClassifierOptions::default();
        let _abort = context.abort_signal().map(|signal| {
            let token = options.request.signal.clone();
            signal.subscribe(move |_| token.cancel())
        });
        Models::classify(self, model, input, options).await
    }

    async fn generate_images(
        &self,
        model: ImageModel,
        input: &ImagesContext,
        context: &Context,
    ) -> AssistantImages {
        let options: ProviderRequestOptions<ImageModel> = ProviderRequestOptions::default();
        let _abort = context.abort_signal().map(|signal| {
            let token = options.signal.clone();
            signal.subscribe(move |_| token.cancel())
        });
        Models::generate_images(self, model, input, options).await
    }
}

fn describe_value(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Array(list)) if list.is_empty() => "an empty array".into(),
        Some(Value::Array(_)) => "an array".into(),
        Some(Value::String(_)) => "a string".into(),
        Some(Value::Number(_)) => "a number".into(),
        Some(Value::Bool(_)) => "a boolean".into(),
        Some(Value::Object(keys)) if keys.is_empty() => "{}".into(),
        Some(Value::Object(keys)) => format!(
            "{{ {}{} }}",
            keys.keys()
                .take(6)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", "),
            if keys.len() > 6 { ", ..." } else { "" }
        ),
    }
}

fn value_json(value: Option<&Value>) -> String {
    value.map_or_else(|| "undefined".into(), Value::to_string)
}

fn model_type(value: Option<&Value>) -> Result<ModelType, CodemodeError> {
    match value.and_then(Value::as_str) {
        Some("chat") => Ok(ModelType::Chat),
        Some("image") => Ok(ModelType::Image),
        Some("classifier") => Ok(ModelType::Classifier),
        _ => Err(CodemodeError::Model(format!(
            "Unknown model type {}. Use \"chat\", \"image\", or \"classifier\".",
            value_json(value)
        ))),
    }
}

fn model_info(mut model: AnyModel) -> Result<Value, CodemodeError> {
    match &mut model {
        AnyModel::Chat(model) => model.headers = None,
        AnyModel::Classifier(model) => model.headers = None,
        AnyModel::Image(model) => model.headers = None,
    }
    Ok(serde_json::to_value(model)?)
}

pub(super) async fn catalog_call(
    name: &str,
    args: Option<&Value>,
    models: &dyn CodemodeModelProvider,
    context: &Context,
) -> Result<Option<Value>, CodemodeError> {
    let list = args
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let kind = list.first();
    let provider = list.get(1);
    if name == "models.getModelOfType" {
        let (Some(provider), Some(id)) = (
            provider.and_then(Value::as_str),
            list.get(2).and_then(Value::as_str),
        ) else {
            let actual = list
                .iter()
                .map(|arg| describe_value(Some(arg)))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(CodemodeError::Model(format!(
                "models.getModelOfType(type, provider, id) expects three strings, got ({actual}). The provider and the id are separate arguments, for example models.getModelOfType(\"classifier\", \"typesafe\", \"jev-latest\")."
            )));
        };
        return models
            .model_of_type(model_type(kind)?, provider, id)
            .map(model_info)
            .transpose();
    }
    let kind = model_type(kind)?;
    let provider = match provider {
        None | Some(Value::Null) => None,
        Some(Value::String(provider)) => Some(provider.as_str()),
        _ => return Err(CodemodeError::Model("provider must be a string".into())),
    };
    let models = if name == "models.getAvailableOfType" {
        models.available_of_type(kind, provider, context).await?
    } else {
        models.models_of_type(kind, provider)
    };
    Ok(Some(Value::Array(
        models
            .into_iter()
            .map(model_info)
            .collect::<Result<_, _>>()?,
    )))
}

const CLASSIFIER_CONTEXT_SHAPE: &str = "{ state: { ... }, images?: [{ type: \"image\", data: <base64>, mimeType }], questions: { <id>: { type: \"choice\", instructions, criteria: { <label>: <meaning> } } | { type: \"score\", instructions, criteria: [<lowest level>, ..., <highest level>] } | { type: \"bool\", instructions, criteria: { true: <meaning>, false: <meaning> } } } }";
const IMAGE_CONTEXT_SHAPE: &str = "{ input: [{ type: \"text\", text: <prompt> }, ...optional { type: \"image\", data: <base64>, mimeType } references] }";

fn is_image(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("image")
        && value.get("data").is_some_and(Value::is_string)
        && value.get("mimeType").is_some_and(Value::is_string)
}

fn check_classifier_context(value: Option<&Value>) -> Result<ClassifierContext, CodemodeError> {
    let fail = |problem: fmt::Arguments<'_>| {
        CodemodeError::Model(format!(
            "models.classify() {problem}. Expected context: {CLASSIFIER_CONTEXT_SHAPE}."
        ))
    };
    let Some(value) = value.filter(|value| value.is_object()) else {
        return Err(fail(format_args!(
            "expects a context object as its second argument, got {}",
            describe_value(value)
        )));
    };
    if !value.get("state").is_some_and(Value::is_object) {
        return Err(fail(format_args!(
            "context.state must be an object, got {}",
            describe_value(value.get("state"))
        )));
    }
    if let Some(images) = value.get("images") {
        let Some(list) = images.as_array() else {
            return Err(fail(format_args!(
                "context.images must be an array, got {}",
                describe_value(Some(images))
            )));
        };
        for (index, image) in list.iter().enumerate() {
            if !is_image(image) {
                return Err(fail(format_args!(
                    "context.images[{index}] must be an image block, got {}",
                    describe_value(Some(image))
                )));
            }
        }
    }
    let questions = value.get("questions");
    let Some(questions) = questions
        .and_then(Value::as_object)
        .filter(|questions| !questions.is_empty())
    else {
        return Err(fail(format_args!(
            "context.questions must map question IDs to questions, got {}",
            describe_value(questions)
        )));
    };
    for (id, question) in questions {
        if !question.is_object() {
            return Err(fail(format_args!(
                "context.questions.{id} must be a question object, got {}",
                describe_value(Some(question))
            )));
        }
        if !question.get("instructions").is_some_and(Value::is_string) {
            return Err(fail(format_args!(
                "context.questions.{id}.instructions must be a string"
            )));
        }
        let criteria = question.get("criteria");
        match question.get("type").and_then(Value::as_str) {
            Some("choice") => {
                if !criteria.and_then(Value::as_object).is_some_and(|values| {
                    !values.is_empty() && values.values().all(Value::is_string)
                }) {
                    return Err(fail(format_args!(
                        "context.questions.{id} is a \"choice\" question, so criteria must map each label to its meaning"
                    )));
                }
            }
            Some("score") => {
                if !criteria
                    .and_then(Value::as_array)
                    .is_some_and(|values| !values.is_empty() && values.iter().all(Value::is_string))
                {
                    return Err(fail(format_args!(
                        "context.questions.{id} is a \"score\" question, so criteria must list the levels as strings, lowest first"
                    )));
                }
            }
            Some("bool") => {
                if !criteria.is_some_and(|value| {
                    ["true", "false"]
                        .iter()
                        .all(|key| value.get(*key).is_some_and(Value::is_string))
                }) {
                    return Err(fail(format_args!(
                        "context.questions.{id} is a \"bool\" question, so criteria must be {{ true: string, false: string }}"
                    )));
                }
            }
            _ => {
                return Err(fail(format_args!(
                    "context.questions.{id}.type must be \"choice\", \"score\", or \"bool\", got {}",
                    value_json(question.get("type"))
                )));
            }
        }
    }
    ClassifierContext::deserialize(value).map_err(CodemodeError::from)
}

fn check_images_context(value: Option<&Value>) -> Result<ImagesContext, CodemodeError> {
    let fail = |problem: fmt::Arguments<'_>| {
        CodemodeError::Model(format!(
            "models.generateImages() {problem}. Expected context: {IMAGE_CONTEXT_SHAPE}."
        ))
    };
    let Some(value) = value.filter(|value| value.is_object()) else {
        return Err(fail(format_args!(
            "expects a context object as its second argument, got {}",
            describe_value(value)
        )));
    };
    let input = value.get("input");
    let Some(input) = input
        .and_then(Value::as_array)
        .filter(|blocks| !blocks.is_empty())
    else {
        return Err(fail(format_args!(
            "context.input must be a non-empty array of blocks, got {}",
            describe_value(input)
        )));
    };
    for (index, block) in input.iter().enumerate() {
        if !(is_image(block)
            || (block.get("type").and_then(Value::as_str) == Some("text")
                && block.get("text").is_some_and(Value::is_string)))
        {
            return Err(fail(format_args!(
                "context.input[{index}] must be a text or image block, got {}",
                describe_value(Some(block))
            )));
        }
    }
    ImagesContext::deserialize(value).map_err(CodemodeError::from)
}

fn type_name(kind: ModelType) -> &'static str {
    match kind {
        ModelType::Chat => "chat",
        ModelType::Image => "image",
        ModelType::Classifier => "classifier",
    }
}

fn resolve_model(
    models: &dyn CodemodeModelProvider,
    name: &str,
    kind: ModelType,
    value: Option<&Value>,
) -> Result<AnyModel, CodemodeError> {
    let article = if kind == ModelType::Image { "an" } else { "a" };
    let name_of_type = type_name(kind);
    let list_hint = format!(
        "List the {name_of_type} models you can use with models.getAvailableOfType(\"{name_of_type}\")."
    );
    let Some((provider, id)) = value
        .and_then(|model| Some((model.get("provider")?.as_str()?, model.get("id")?.as_str()?)))
    else {
        let hint = if value.is_none_or(Value::is_null) {
            " models.getModelOfType() returns undefined for an unknown provider or id."
        } else {
            ""
        };
        return Err(CodemodeError::Model(format!(
            "{name}() expects {article} {name_of_type} model as its first argument, got {}.{hint} {list_hint}",
            describe_value(value)
        )));
    };
    if let Some(model) = models.model_of_type(kind, provider, id) {
        return Ok(model);
    }
    let actual = [ModelType::Chat, ModelType::Image, ModelType::Classifier]
        .into_iter()
        .find(|other| *other != kind && models.model_of_type(*other, provider, id).is_some());
    let problem = if let Some(actual) = actual {
        let actual_article = if actual == ModelType::Image {
            "an"
        } else {
            "a"
        };
        format!(
            "\"{provider}/{id}\" is {actual_article} {} model, not {article} {name_of_type} model.",
            type_name(actual)
        )
    } else {
        format!("Unknown {name_of_type} model \"{provider}/{id}\".")
    };
    Err(CodemodeError::Model(format!("{problem} {list_hint}")))
}

pub fn create_model_tools() -> [CodemodeToolSpec; 2] {
    MODEL_CALLS.map(|(global, name)| CodemodeToolSpec {
        name: name.into(),
        description: format!("{global}() of code mode scripts."),
        parameters: json!({"type": "object", "properties": {"model": {}, "context": {}}, "required": ["model", "context"]}),
        structured_output_schema: Some(json!({})),
        deferred: false,
    })
}

/// Result to record in a nested task. The nested executor owns usage accounting and replay.
#[derive(Clone, Debug, PartialEq)]
pub struct CodemodeModelToolResult {
    pub output: Vec<InputContent>,
    pub nested: CodemodeNestedResult,
}

fn model_result(
    label: &str,
    result: &impl Serialize,
    stop: StopReason,
    error: Option<&str>,
    usage: Option<Usage>,
) -> Result<CodemodeModelToolResult, CodemodeError> {
    let ok = stop == StopReason::Stop;
    let output = if ok {
        format!("{label} done")
    } else {
        format!("{label} {}: {}", stop.as_str(), error.unwrap_or_default())
    };
    Ok(CodemodeModelToolResult {
        output: vec![InputContent::Text(output.into())],
        nested: CodemodeNestedResult {
            structured_output: Some(serde_json::to_value(result)?),
            is_error: !ok,
            errors: Vec::new(),
            usage,
            duration_ms: None,
        },
    })
}

pub(super) async fn execute_model_tool(
    models: &dyn CodemodeModelProvider,
    name: &str,
    arguments: &Value,
    context: &Context,
) -> Result<CodemodeModelToolResult, CodemodeError> {
    let model_arg = arguments.get("model");
    let input = arguments.get("context");
    match name {
        CLASSIFY_TOOL => {
            let AnyModel::Classifier(model) =
                resolve_model(models, "models.classify", ModelType::Classifier, model_arg)?
            else {
                return Err(CodemodeError::Model(
                    "The provider did not resolve a classifier model".into(),
                ));
            };
            let checked = check_classifier_context(input)?;
            let label = format!("classify {}/{}", model.provider, model.id);
            let result = models.classify(model, &checked, context).await;
            model_result(
                &label,
                &result,
                result.stop_reason,
                result.error_message.as_deref(),
                result.usage,
            )
        }
        GENERATE_IMAGES_TOOL => {
            let AnyModel::Image(model) =
                resolve_model(models, "models.generateImages", ModelType::Image, model_arg)?
            else {
                return Err(CodemodeError::Model(
                    "The provider did not resolve an image model".into(),
                ));
            };
            let checked = check_images_context(input)?;
            let label = format!("generateImages {}/{}", model.provider, model.id);
            let result = models.generate_images(model, &checked, context).await;
            model_result(
                &label,
                &result,
                result.stop_reason,
                result.error_message.as_deref(),
                result.usage,
            )
        }
        _ => Err(CodemodeError::Model(format!(
            "Unknown codemode model tool {name}"
        ))),
    }
}
