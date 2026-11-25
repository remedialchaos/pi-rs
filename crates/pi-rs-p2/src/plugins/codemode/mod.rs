//! `codemode` built-in plugin: installs Durable's `codemode` extension backed by
//! `pi-rs-codemode::CodemodeSandbox` (Wasmtime + QuickJS) and reacts live to `settings.codemode`.

use std::sync::Arc;

use pi_rs_ai::InputContent;
use pi_rs_chord::{
    Error, Result,
    host::{Members, ServeOptions, define_facet},
};
use pi_rs_codemode::{CodemodeSandbox, ExecuteOptions, SandboxOptions};
use pi_rs_durable::{
    codemode::{
        CODEMODE_TOOL_NAME, CodemodeToolInput, CodemodeToolOptions, create_codemode_extension,
    },
    harness::{Extension, ToolRegistration, define_extension},
    tools::ToolResult,
};
use serde_json::{Value, json};

use crate::api::v1::{FacetKind, PluginRegistry, p2_contracts::lookup, worker::worker_of};

/// Default memory limit for p2 codemode scripts (matches upstream `CODEMODE_MEMORY_LIMIT_BYTES`).
pub const CODEMODE_MEMORY_LIMIT_BYTES: usize = 256 * 1024 * 1024;

/// Builds the Durable `codemode` extension backed by `sandbox`.
pub fn build_codemode_extension(sandbox: CodemodeSandbox) -> Extension {
    let cm = create_codemode_extension(CodemodeToolOptions::new(sandbox.clone()));
    let declaration = cm.tool.declaration();
    let registration = ToolRegistration {
        declaration,
        sequential: false,
        prepare_arguments: None,
        cancel: None,
        structured_output_schema: None,
        output_limits: None,
        replay_safe: true,
        callers: Some(vec!["model".into()]),
        deferred: false,
        execute: Arc::new(move |args, _invocation, _context| {
            let sandbox = sandbox.clone();
            Box::pin(async move {
                let input: CodemodeToolInput = serde_json::from_value(args)?;
                match sandbox
                    .execute(&input.code, ExecuteOptions::default())
                    .await
                {
                    Ok(outcome) => {
                        let value = outcome.value().cloned().unwrap_or(Value::Null);
                        let text = serde_json::to_string_pretty(&value)
                            .unwrap_or_else(|_| value.to_string());
                        Ok(ToolResult {
                            output: Some(vec![InputContent::Text(text.into())]),
                            details: Some(json!({ "ok": true })),
                            ..Default::default()
                        }
                        .into())
                    }
                    Err(err) => Ok(ToolResult {
                        output: Some(vec![InputContent::Text(err.to_string().into())]),
                        is_error: Some(true),
                        details: Some(json!({ "ok": false })),
                        ..Default::default()
                    }
                    .into()),
                }
            })
        }),
    };

    define_extension(Extension {
        name: CODEMODE_TOOL_NAME.into(),
        tools: vec![registration],
        ..Default::default()
    })
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let runtime_contract = Arc::new(lookup("p2.codemode-runtime")?);
    registry.register(
        "codemode",
        FacetKind::Worker,
        define_facet(move |host| {
            host.serve(
                runtime_contract.clone(),
                move |_, _, object| {
                    if let Ok(worker) = worker_of(&object.host()) {
                        let sandbox = CodemodeSandbox::new(SandboxOptions {
                            memory_limit_bytes: Some(CODEMODE_MEMORY_LIMIT_BYTES),
                            ..Default::default()
                        })
                        .map_err(|e| Error::Invalid(e.to_string()))?;
                        let extension = build_codemode_extension(sandbox);
                        let ext_name = extension.name.clone();
                        let registries = worker.registries.clone();
                        registries.extensions.install(extension)?;
                        let initial_enabled = registries
                            .settings
                            .borrow()
                            .get("codemode")
                            .and_then(Value::as_bool)
                            != Some(false);
                        let _ = registries
                            .extensions
                            .switch(&ext_name, Some(initial_enabled));

                        let settings_rx = registries.settings.subscribe();
                        let switch_regs = registries.clone();
                        let switch_name = ext_name.clone();
                        object.on_live(move |ctx| {
                            let switch_regs = switch_regs.clone();
                            let switch_name = switch_name.clone();
                            let mut rx = settings_rx.clone();
                            Box::pin(async move {
                                while matches!(
                                    ctx.context.await_with(rx.changed()).await,
                                    Ok(Ok(()))
                                ) {
                                    let enabled = rx
                                        .borrow_and_update()
                                        .get("codemode")
                                        .and_then(Value::as_bool)
                                        != Some(false);
                                    let _ =
                                        switch_regs.extensions.switch(&switch_name, Some(enabled));
                                }
                                Ok(())
                            })
                        });

                        let _cleanup = object.open(
                            |_| Box::pin(async { Ok(()) }),
                            move |_, _| {
                                let registries = registries.clone();
                                let ext_name = ext_name.clone();
                                Box::pin(async move {
                                    let _ = registries.extensions.switch(&ext_name, None);
                                    let _ = registries.extensions.uninstall(&ext_name);
                                    Ok(())
                                })
                            },
                        );
                    }
                    Ok(Members::new())
                },
                ServeOptions::default(),
            )
        }),
    )
}
