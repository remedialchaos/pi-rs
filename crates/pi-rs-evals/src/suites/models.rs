use super::*;
pub(super) fn case() -> Case {
    Case::new(
        "models",
        "Add model to existing provider",
        "adds the model without replacing existing models",
    )
}
pub(super) fn input() -> PiCodingAgentInput {
    steps("Configure this running Pi installation with a new `openai/fixture-chat` model. Do not create project-local configuration. Show it as “Fixture Chat”. It accepts text, supports reasoning, has a 32,768-token context window and a 4,096-token maximum output, and has no usage cost.".into(),None)
}
