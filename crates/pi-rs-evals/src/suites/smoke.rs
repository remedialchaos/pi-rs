use super::*;
pub(super) fn case() -> Case {
    Case::new(
        "smoke",
        "Answer a basic prompt",
        "returns the expected answer",
    )
}
pub(super) fn input() -> PiCodingAgentInput {
    "What's the capital of France? Respond with only the city name.".into()
}
pub(super) fn judge(run: &SimpleHarnessResult<Value>, model: &PiCodingAgentModelSelection) -> bool {
    run.output.as_str().is_some_and(|s| s.trim() == "Paris")
        && run.errors.is_empty()
        && run.usage.provider.as_ref() == Some(&model.provider)
        && run.usage.model.as_ref() == Some(&model.id)
        && run.usage.total_tokens.is_some_and(|n| n > 0.0)
}
