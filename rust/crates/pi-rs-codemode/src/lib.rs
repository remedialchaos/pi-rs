#![forbid(unsafe_code)]
//! Isolated JavaScript execution and model-facing declaration rendering.
pub mod guest;
pub mod remote;
pub use remote::*;
mod runtime;
pub mod types;
pub use guest::{WasmModule, load_quickjs_wasm};
pub use runtime::{
    CodemodeRun, CodemodeSandbox, DEFAULT_INTERRUPT_BUDGET, DeclaredTool, INLINE_MAX_STACK_SIZE,
    JOBS_PER_POLL, MAX_OUTPUT_CHARS, MAX_OUTPUT_ITEMS, MAX_STORE_TOTAL_CHARS,
    MAX_STORE_VALUE_CHARS, Outcome, RunCall, RunController, RunOptions,
};
pub use types::*;
pub mod identifier;
pub mod schema;
pub mod source;
#[cfg(test)]
mod tests {
    #[test]
    fn a_host_function_can_resolve_an_awaited_promise() -> Result<(), crate::HostError> {
        let host = crate::CodemodeTool::new("host", |_, _| async { Ok(Some(42.into())) });
        let sandbox = crate::CodemodeSandbox::new(crate::SandboxOptions {
            tools: vec![host],
            ..Default::default()
        })?;
        let result = futures::executor::block_on(sandbox.execute(
            "return await tools.host()",
            crate::ExecuteOptions::default(),
        ))?;
        assert_eq!(result.value(), Some(&42.into()), "{result:?}");
        Ok(())
    }
}
