use std::sync::Arc;

use pi_rs_coding_agent::core::event_bus::EventBus;
use pi_rs_coding_agent::core::extensions::context::default_command_actions;
use pi_rs_coding_agent::core::extensions::types::{
    Extension, ExtensionCommandContext, ExtensionContext, ExtensionHost, ExtensionInitContext,
    ExtensionMode, ExtensionRegistration, ExtensionToolContext, ExtensionUi,
};
use pi_rs_coding_agent::core::extensions::wasm::{WasmExtension, wit_bindings};
use serde_json::json;

const ECHO_COMPONENT: &[u8] = include_bytes!("fixtures/echo_extension.wasm");

fn test_base_context() -> (Arc<EventBus>, Arc<ExtensionHost>, ExtensionContext) {
    let bus = Arc::new(EventBus::new());
    let host = Arc::new(ExtensionHost::new(bus.clone()));
    let ctx = ExtensionContext {
        host: host.clone(),
        cwd: "/work".to_string(),
        mode: ExtensionMode::Print,
        ui: ExtensionUi::no_op(ExtensionMode::Print),
        bus: Some(bus.clone()),
        is_idle: Arc::new(|| true),
        is_project_trusted: Arc::new(|| true),
        has_pending_messages: Arc::new(|| false),
        get_context_usage: Arc::new(|| None),
        get_system_prompt: Arc::new(String::new),
        abort: Arc::new(|| {}),
        shutdown: Arc::new(|| {}),
        compact: Arc::new(|_| {}),
    };
    (bus, host, ctx)
}

#[tokio::test]
async fn wit_wasm_component_executes_init_patches_tools_commands_and_reload_invalidation() {
    // Compile-time check that the checked-in WIT world generated real bindings.
    let _ = std::any::type_name::<wit_bindings::Extension>();

    let mut extension = WasmExtension::new(
        "echo-wasm",
        Some(ECHO_COMPONENT),
        ExtensionRegistration::new("echo-wasm"),
        false,
        false,
    )
    .expect("load real WIT component fixture");
    extension.set_flag("mode", "strict");
    extension.set_tool_handler(Arc::new(|name, args_json| {
        Ok(format!(r#"{{"host_ran":"{name}","received":{args_json}}}"#))
    }));

    assert!(extension.store().lock().expect("lock").has_store());
    assert!(extension.store().lock().expect("lock").instantiated());

    let (bus, host, base_ctx) = test_base_context();
    let mut reg = ExtensionRegistration::new("echo-wasm");
    let mut init_ctx = ExtensionInitContext {
        registration: &mut reg,
        host: &host,
        bus: &bus,
        cwd: "/work",
    };
    extension.init(&mut init_ctx).await.expect("guest init");
    assert_eq!(
        extension.notifications(),
        vec![("info".to_string(), "init:strict".to_string())]
    );

    let blocked = extension
        .call_on_tool_call("blocked", &json!({"cmd": "rm -rf /"}))
        .await
        .expect("on_tool_call blocked")
        .expect("block patch returned");
    assert_eq!(blocked.block.value, Some(true));
    assert_eq!(blocked.reason.value.as_deref(), Some("blocked by wasm"));

    let rewritten = extension
        .call_on_tool_call("rewrite", &json!({"orig": 1}))
        .await
        .expect("on_tool_call rewrite")
        .expect("rewrite patch returned");
    assert_eq!(rewritten.input.value, Some(json!({"rewritten": true})));

    let passthrough = extension
        .call_on_tool_call("other", &json!({}))
        .await
        .expect("on_tool_call other");
    assert!(passthrough.is_none());

    let headers_patch = extension
        .call_on_before_headers(&json!({"x-drop": "old", "x-keep": "1"}))
        .await
        .expect("on_before_headers")
        .expect("headers patch returned");
    let mut headers = json!({"x-drop": "old", "x-keep": "1"});
    headers_patch.apply(&mut headers);
    assert_eq!(headers, json!({"x-keep": "1", "x-wasm-ext": "active"}));

    let tool_ctx = ExtensionToolContext::new(base_ctx, "call-1");
    let echo = extension
        .execute_tool("echo", json!({"n": 42}), &tool_ctx)
        .await
        .expect("guest execute_tool echo");
    assert!(!echo.is_error);
    assert_eq!(echo.result, json!({"tool": "echo", "args": {"n": 42}}));

    let delegated = extension
        .execute_tool("delegate", json!({"x": 7}), &tool_ctx)
        .await
        .expect("guest execute_tool delegate");
    assert!(!delegated.is_error);
    assert_eq!(
        delegated.result,
        json!({"host_ran": "nested", "received": {"x": 7}})
    );

    let (_, _, cmd_base_ctx) = test_base_context();
    let (wait_for_idle, new_session, fork, navigate_tree, switch_session, reload) =
        default_command_actions();
    let cmd_ctx = ExtensionCommandContext {
        base: cmd_base_ctx,
        wait_for_idle,
        new_session,
        fork,
        navigate_tree,
        switch_session,
        reload,
    };
    extension
        .run_command("ping", "world", &cmd_ctx)
        .await
        .expect("guest run_command ping");
    assert_eq!(
        extension.notifications().last(),
        Some(&("command".to_string(), "pong:world".to_string()))
    );

    // /reload invalidation drops the live wasmtime::Store, bumps generation,
    // and rejects every subsequent guest export with the stale-context error.
    let gen_before = extension.generation();
    extension.invalidate();
    assert_eq!(extension.generation(), gen_before + 1);
    assert!(!extension.store().lock().expect("lock").has_store());
    assert!(
        extension
            .call_on_tool_call("blocked", &json!({}))
            .await
            .expect_err("stale tool_call")
            .contains("stale")
    );
    assert!(
        extension
            .execute_tool("echo", json!({}), &tool_ctx)
            .await
            .expect_err("stale execute_tool")
            .contains("stale")
    );
    assert!(
        extension
            .run_command("ping", "after", &cmd_ctx)
            .await
            .expect_err("stale run_command")
            .contains("stale")
    );
}
