use super::*;
pub(super) fn case() -> Case {
    Case::new(
        "custom_provider",
        "Add custom streaming provider",
        "implements a provider from an API fixture",
    )
}
pub(super) fn input(fixture: &acme_server::Server) -> PiCodingAgentInput {
    steps(
        format!(
            "Configure this running Pi installation with Acme Stream as a provider. Do not create a project package or modify project source. Its provider ID is acme-stream, its API is at {}, and its API documentation is in ./fixtures/acme-stream-api.json. Read its credential from the ACME_STREAM_API_KEY environment variable.\n\nIt offers one model, acme-stream-chat, shown as “Acme Stream Chat”. The model accepts text, does not support reasoning, has a 16,384-token context window and a 2,048-token maximum output, and has no usage cost.",
            fixture.origin
        ),
        None,
    )
}
