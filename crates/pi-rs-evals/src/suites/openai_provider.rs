use super::*;
pub(super) fn case() -> Case {
    Case::new(
        "openai_provider",
        "Add OpenAI-compatible provider",
        "configures a provider that works through Pi",
    )
}
pub(super) fn input(fixture: &acme_server::Server) -> PiCodingAgentInput {
    steps(
        format!(
            "Configure this running Pi installation with Acme as a provider. Do not create project-local configuration. Its provider ID is acme, its API is at {}/v1, and it uses OpenAI Chat Completions. Read its API key from the ACME_API_KEY environment variable.\n\nThe provider offers one model, acme-chat, shown as “Acme Chat”. It accepts text, does not support reasoning, has a 32,768-token context window and a 4,096-token maximum output, and has no usage cost.",
            fixture.origin
        ),
        None,
    )
}
