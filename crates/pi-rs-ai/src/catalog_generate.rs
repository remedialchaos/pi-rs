//! Pure-Rust catalog generator, like upstream's `packages/ai/catalog/generate.ts`: it reads a
//! hydrated model-data directory and writes the catalog assets the crate embeds.
//!
//! A hydrated directory holds one `<provider>.json` file per provider, each mapping a model
//! group to its models, like upstream's `src/providers/data/<provider>.json`.

use std::path::Path;

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::error::{Error, Result};

/// The model groups of one hydrated provider file: group name to model id to model value.
type Groups = IndexMap<String, Map<String, Value>>;

/// A generated catalog, split by model type.
#[derive(Clone, Debug, Default)]
pub struct GeneratedCatalog {
    /// Chat models by provider.
    pub chat: IndexMap<String, IndexMap<String, Value>>,
    /// Image models by provider.
    pub images: IndexMap<String, IndexMap<String, Value>>,
    /// Classifier models by provider.
    pub classifiers: IndexMap<String, IndexMap<String, Value>>,
}

/// Counts of a generated catalog.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CatalogSummary {
    /// Providers in the catalog.
    pub providers: usize,
    /// Chat models in the catalog.
    pub chat_models: usize,
    /// Chat models that support reasoning.
    pub reasoning_models: usize,
    /// Image models in the catalog.
    pub image_models: usize,
    /// Classifier models in the catalog.
    pub classifier_models: usize,
}
/// The file names of the generated catalog assets.
pub const CHAT_MODELS_FILE: &str = "chat-models.json";
pub const IMAGE_MODELS_FILE: &str = "image-models.json";
pub const CLASSIFIER_MODELS_FILE: &str = "classifier-models.json";
pub const PROVIDERS_FILE: &str = "providers.json";

/// Reads a hydrated model-data directory and groups its models by type.
pub fn generate(hydrated_dir: &Path) -> Result<GeneratedCatalog> {
    let mut catalog = GeneratedCatalog::default();
    let mut entries = std::fs::read_dir(hydrated_dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|suffix| suffix.to_str()) != Some("json") {
            continue;
        }
        let Some(provider) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if provider.starts_with('.') {
            continue;
        }
        let file = std::fs::File::open(&path)?;
        let groups: Groups = serde_json::from_reader(std::io::BufReader::new(file))?;
        catalog.chat.entry(provider.to_owned()).or_default();
        catalog.classifiers.entry(provider.to_owned()).or_default();
        for (group, models) in groups {
            for (key, value) in models {
                let kind = value
                    .get("type")
                    .and_then(Value::as_str)
                    .or_else(|| key.split_once(':').map(|(kind, _)| kind))
                    .unwrap_or(&group);
                let target = match kind {
                    "chat" => &mut catalog.chat,
                    "image" => &mut catalog.images,
                    "classifier" => &mut catalog.classifiers,
                    _ => continue,
                };
                let id = value
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        Error::ModelSource(format!("catalog model in {path:?} is missing id"))
                    })?
                    .to_owned();
                target
                    .entry(provider.to_owned())
                    .or_default()
                    .insert(id, value);
            }
        }
    }
    Ok(catalog)
}

/// Counts of a generated catalog.
pub fn summarize(catalog: &GeneratedCatalog) -> CatalogSummary {
    let chat_models: Vec<&Value> = catalog
        .chat
        .values()
        .flat_map(|models| models.values())
        .collect();
    CatalogSummary {
        providers: catalog.chat.len(),
        chat_models: chat_models.len(),
        reasoning_models: chat_models
            .iter()
            .filter(|model| model.get("reasoning").and_then(Value::as_bool) == Some(true))
            .count(),
        image_models: catalog.images.values().map(IndexMap::len).sum(),
        classifier_models: catalog.classifiers.values().map(IndexMap::len).sum(),
    }
}

/// The generated catalog as the JSON assets the crate embeds: one provider per line, with the
/// provider's models compact, like the checked-in assets.
fn assets(catalog: &GeneratedCatalog) -> [(String, String); 3] {
    let write = |value: &IndexMap<String, IndexMap<String, Value>>| {
        let mut out = String::from("{\n");
        for (index, (provider, models)) in value.iter().enumerate() {
            let name = serde_json::to_string(provider).unwrap_or_default();
            let models = serde_json::to_string(models).unwrap_or_default();
            out.push_str("  ");
            out.push_str(&name);
            out.push_str(": ");
            out.push_str(&models);
            if index + 1 < value.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str("}\n");
        out
    };
    [
        (CHAT_MODELS_FILE.to_owned(), write(&catalog.chat)),
        (IMAGE_MODELS_FILE.to_owned(), write(&catalog.images)),
        (
            CLASSIFIER_MODELS_FILE.to_owned(),
            write(&catalog.classifiers),
        ),
    ]
}

/// Writes the generated catalog assets into `out_dir`.
pub fn write(catalog: &GeneratedCatalog, out_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(out_dir)?;
    for (name, contents) in assets(catalog) {
        std::fs::write(out_dir.join(name), contents)?;
    }
    Ok(())
}

/// Checks that the assets in `out_dir` are the ones a generation run would write.
pub fn check(catalog: &GeneratedCatalog, out_dir: &Path) -> Result<()> {
    for (name, contents) in assets(catalog) {
        let path = out_dir.join(&name);
        let current = std::fs::read_to_string(&path).map_err(|error| {
            Error::ModelSource(format!("cannot read {}: {error}", path.display()))
        })?;
        if current != contents {
            return Err(Error::ModelSource(format!(
                "{} is out of date",
                path.display()
            )));
        }
    }
    Ok(())
}
