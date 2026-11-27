//! The pure-Rust catalog generator: it turns a hydrated model-data directory into the catalog
//! assets the crate embeds.

use std::fs;
use std::path::PathBuf;

use pi_rs_ai::catalog_generate::{self, CatalogSummary};
use serde_json::{Value, json};

fn temp_dir(label: &str) -> std::io::Result<PathBuf> {
    let path =
        std::env::temp_dir().join(format!("pi-rs-ai-catalog-{label}-{}", uuid::Uuid::now_v7()));

    fs::create_dir_all(&path)?;
    Ok(path)
}

fn hydrated_model(id: &str, kind: &str, api: &str) -> Value {
    json!({
        "id": id,
        "name": id,
        "api": api,
        "provider": "acme",
        "baseUrl": "https://api.acme.test",
        "reasoning": false,
        "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 1024,
        "maxTokens": 256,
        "type": kind,
    })
}

#[test]
fn the_generator_groups_models_by_type_and_writes_the_assets()
-> Result<(), Box<dyn std::error::Error>> {
    let root = temp_dir("grouped")?;
    let hydrated = root.join("hydrated");
    fs::create_dir_all(&hydrated)?;
    fs::write(
        hydrated.join("acme.json"),
        serde_json::to_vec(&json!({
            "chat": {"acme-chat": hydrated_model("acme-chat", "chat", "openai-completions")},
            "image": {"acme-image": hydrated_model("acme-image", "image", "openrouter-images")},
            "classifier": {"acme-classifier": hydrated_model("acme-classifier", "classifier", "openai-decisions")},
        }))?,
    )?;

    let catalog = catalog_generate::generate(&hydrated)?;
    assert_eq!(catalog.chat.len(), 1);
    assert!(catalog.chat["acme"].contains_key("acme-chat"));
    assert!(catalog.images["acme"].contains_key("acme-image"));
    assert!(catalog.classifiers["acme"].contains_key("acme-classifier"));

    let summary = catalog_generate::summarize(&catalog);
    assert_eq!(
        summary,
        CatalogSummary {
            providers: 1,
            chat_models: 1,
            reasoning_models: 0,
            image_models: 1,
            classifier_models: 1,
        }
    );

    let out = root.join("assets");
    catalog_generate::write(&catalog, &out)?;
    let chat = fs::read_to_string(out.join(catalog_generate::CHAT_MODELS_FILE))?;
    assert!(chat.starts_with("{\n  \"acme\": {"), "{chat}");
    assert!(chat.contains("\"acme-chat\""));
    assert!(chat.ends_with("}\n"));

    // The written assets are the ones a generation run produces.
    catalog_generate::check(&catalog, &out)?;
    fs::write(out.join(catalog_generate::CHAT_MODELS_FILE), "{}\n")?;
    assert!(catalog_generate::check(&catalog, &out).is_err());

    fs::remove_dir_all(&root)?;
    Ok(())
}

#[test]
fn the_generator_ignores_files_that_are_not_provider_data() -> Result<(), Box<dyn std::error::Error>>
{
    let root = temp_dir("ignored")?;
    let hydrated = root.join("hydrated");
    fs::create_dir_all(&hydrated)?;
    fs::write(hydrated.join("notes.txt"), "not a provider")?;
    fs::write(hydrated.join(".hidden.json"), "{}")?;
    let catalog = catalog_generate::generate(&hydrated)?;
    assert!(catalog.chat.is_empty());
    assert!(catalog.images.is_empty());
    assert!(catalog.classifiers.is_empty());
    fs::remove_dir_all(&root)?;
    Ok(())
}

#[test]
fn the_generator_reports_a_model_without_an_id() -> Result<(), Box<dyn std::error::Error>> {
    let root = temp_dir("no-id")?;
    let hydrated = root.join("hydrated");
    fs::create_dir_all(&hydrated)?;
    fs::write(
        hydrated.join("acme.json"),
        serde_json::to_vec(&json!({"chat": {"broken": {"name": "no id"}}}))?,
    )?;
    let error = catalog_generate::generate(&hydrated)
        .err()
        .ok_or("accepted model without an id")?;
    assert!(error.to_string().contains("is missing id"), "{error}");
    fs::remove_dir_all(&root)?;
    Ok(())
}
