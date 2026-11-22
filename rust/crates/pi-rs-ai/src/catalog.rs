use std::path::Path;
use std::sync::OnceLock;

use indexmap::IndexMap;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::model::{AnyModel, ClassifierModel, ImageModel, Model, ModelType};

pub type ProviderModels<M> = IndexMap<String, M>;

#[derive(Debug)]
pub struct Catalog {
    chat: IndexMap<String, ProviderModels<Model>>,
    images: IndexMap<String, ProviderModels<ImageModel>>,
    classifiers: IndexMap<String, ProviderModels<ClassifierModel>>,
}

impl Catalog {
    pub fn embedded() -> Result<&'static Self> {
        static CATALOG: OnceLock<std::result::Result<Catalog, String>> = OnceLock::new();
        CATALOG
            .get_or_init(|| Self::embedded_data().map_err(|error| error.to_string()))
            .as_ref()
            .map_err(|e| Error::ModelSource(e.clone()))
    }

    fn embedded_data() -> Result<Self> {
        Ok(Self {
            chat: serde_json::from_str(include_str!("../assets/chat-models.json"))?,
            images: serde_json::from_str(include_str!("../assets/image-models.json"))?,
            classifiers: serde_json::from_str(include_str!("../assets/classifier-models.json"))?,
        })
    }

    pub fn from_hydrated_dir(path: &Path) -> Result<Self> {
        let mut catalog = Self {
            chat: IndexMap::new(),
            images: IndexMap::new(),
            classifiers: IndexMap::new(),
        };
        let mut entries = std::fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_unstable_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(provider) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if provider.starts_with('.') {
                continue;
            }
            let data = std::fs::File::open(&path)?;
            let groups: IndexMap<String, Map<String, Value>> =
                serde_json::from_reader(std::io::BufReader::new(data))?;
            let mut chat = IndexMap::new();
            let mut images = IndexMap::new();
            let mut classifiers = IndexMap::new();
            for (key, value) in groups.into_values().flat_map(Map::into_iter) {
                let kind = value
                    .get("type")
                    .and_then(Value::as_str)
                    .or_else(|| key.split_once(':').map(|(kind, _)| kind));
                match kind {
                    Some("chat") => insert(&mut chat, value)?,
                    Some("image") => insert(&mut images, value)?,
                    Some("classifier") => insert(&mut classifiers, value)?,
                    _ => {}
                }
            }
            catalog.chat.insert(provider.to_owned(), chat);
            if !images.is_empty() {
                catalog.images.insert(provider.to_owned(), images);
            }
            catalog.classifiers.insert(provider.to_owned(), classifiers);
        }
        Ok(catalog)
    }

    pub fn providers(&self) -> impl Iterator<Item = &str> {
        self.chat.keys().map(String::as_str)
    }
    pub fn image_providers(&self) -> impl Iterator<Item = &str> {
        self.images.keys().map(String::as_str)
    }
    pub fn chat_models(&self, provider: &str) -> Option<&ProviderModels<Model>> {
        self.chat.get(provider)
    }
    pub fn image_models(&self, provider: &str) -> Option<&ProviderModels<ImageModel>> {
        self.images.get(provider)
    }
    pub fn classifier_models(&self, provider: &str) -> Option<&ProviderModels<ClassifierModel>> {
        self.classifiers.get(provider)
    }
    pub fn get_model(&self, provider: &str, id: &str) -> Option<&Model> {
        self.chat_models(provider)?.get(id)
    }
    pub fn get_image_model(&self, provider: &str, id: &str) -> Option<&ImageModel> {
        self.image_models(provider)?.get(id)
    }
    pub fn get_classifier_model(&self, provider: &str, id: &str) -> Option<&ClassifierModel> {
        self.classifier_models(provider)?.get(id)
    }

    pub fn all_provider_models(&self, provider: &str) -> impl Iterator<Item = AnyModel> + '_ {
        self.chat_models(provider)
            .into_iter()
            .flat_map(|m| m.values().cloned().map(AnyModel::Chat))
            .chain(
                self.image_models(provider)
                    .into_iter()
                    .flat_map(|m| m.values().cloned().map(AnyModel::Image)),
            )
            .chain(
                self.classifier_models(provider)
                    .into_iter()
                    .flat_map(|m| m.values().cloned().map(AnyModel::Classifier)),
            )
    }
}

fn insert<T: DeserializeOwned>(out: &mut ProviderModels<T>, value: Value) -> Result<()> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::ModelSource("catalog model is missing id".into()))?
        .to_owned();
    out.insert(id, serde_json::from_value(value)?);
    Ok(())
}

pub fn models_are_equal(a: Option<&AnyModel>, b: Option<&AnyModel>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            a.model_type() == b.model_type() && a.id() == b.id() && a.provider() == b.provider()
        }
        _ => false,
    }
}

pub fn get_model_type(model: &AnyModel) -> ModelType {
    model.model_type()
}
