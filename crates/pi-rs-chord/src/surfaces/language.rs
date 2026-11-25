//! Locale selection and message catalogs, without process-global locale or browser state.
use crate::{
    Error, Result,
    context::Context,
    context::lock,
    json::JsonValue,
    state::{Batch, NativeObservation, NativeSource},
    watch::NativeWatchCall,
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
/// Canonicalizes well-formed BCP 47 tags. CLDR alias/likely-subtag data belongs to the platform adapter.
pub fn canonical_locale(locale: &str) -> Option<String> {
    let parts = locale.split('-').collect::<Vec<_>>();
    let first = *parts.first()?;
    if !(2..=8).contains(&first.len()) || !first.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let language = match first.to_ascii_lowercase().as_str() {
        "iw" => "he".into(),
        "in" => "id".into(),
        "ji" => "yi".into(),
        other => other.into(),
    };
    let mut out = vec![language];
    let mut seen = BTreeSet::new();
    let mut extension = false;
    for (index, part) in parts.iter().enumerate().skip(1) {
        if part.is_empty() || part.len() > 8 || !part.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        let lower = part.to_ascii_lowercase();
        if part.len() == 1 {
            if index + 1 == parts.len() || !seen.insert(lower.clone()) {
                return None;
            }
            extension = true;
            out.push(lower);
        } else if !extension && part.len() == 4 && part.bytes().all(|b| b.is_ascii_alphabetic()) {
            let mut value = lower;
            value[..1].make_ascii_uppercase();
            out.push(value);
        } else if !extension
            && ((part.len() == 2 && part.bytes().all(|b| b.is_ascii_alphabetic()))
                || (part.len() == 3 && part.bytes().all(|b| b.is_ascii_digit())))
        {
            out.push(part.to_ascii_uppercase());
        } else {
            if !extension
                && !(part.len() >= 5 || (part.len() == 4 && part.as_bytes()[0].is_ascii_digit()))
            {
                return None;
            }
            out.push(lower);
        }
    }
    Some(out.join("-"))
}
pub fn locale_chain(locale: &str) -> Vec<String> {
    let mut parts = locale.split('-').collect::<Vec<_>>();
    let mut result = Vec::new();
    while !parts.is_empty() {
        result.push(parts.join("-"));
        parts.pop();
    }
    result
}
pub fn match_locale(preferred: &[String], offered: &[String]) -> Option<String> {
    for wanted in preferred {
        let Some(locale) = canonical_locale(wanted) else {
            continue;
        };
        for candidate in locale_chain(&locale) {
            if offered.contains(&candidate) {
                return Some(candidate);
            }
        }
        let base = locale.split('-').next();
        if let Some(found) = offered.iter().find(|each| each.split('-').next() == base) {
            return Some(found.clone());
        }
    }
    None
}
pub fn resolve_locale(pick: Option<&str>, preferred: &[String], offered: &[String]) -> String {
    match_locale(
        &pick.map(String::from).into_iter().collect::<Vec<_>>(),
        offered,
    )
    .or_else(|| match_locale(preferred, offered))
    .or_else(|| offered.iter().find(|l| l.as_str() == "en").cloned())
    .or_else(|| offered.first().cloned())
    .unwrap_or_else(|| "en".into())
}
pub fn direction_of(locale: &str) -> &'static str {
    let script = locale.split('-').find(|part| part.len() == 4);
    let rtl = match script {
        Some(script) => [
            "Adlm", "Arab", "Hebr", "Mand", "Nkoo", "Rohg", "Samr", "Syrc", "Thaa", "Yezi",
        ]
        .contains(&script),
        None => [
            "ar", "ur", "fa", "he", "yi", "ps", "sd", "ug", "dv", "syr", "nqo",
        ]
        .contains(&locale.split('-').next().unwrap_or("")),
    };
    if rtl { "rtl" } else { "ltr" }
}
type MessageFunction = Arc<dyn Fn(&[JsonValue]) -> Result<String> + Send + Sync>;
#[derive(Clone)]
pub enum MessageValue {
    Text(String),
    Function(MessageFunction),
}
impl MessageValue {
    fn render(&self, args: &[JsonValue]) -> Result<String> {
        match self {
            Self::Text(value) => Ok(value.clone()),
            Self::Function(f) => f(args),
        }
    }
}
pub type Catalog = IndexMap<String, MessageValue>;
#[derive(Clone)]
pub struct Translation {
    pub namespace: String,
    pub locale: String,
    pub messages: Arc<Catalog>,
    pub override_catalog: bool,
}
#[derive(Clone)]
pub struct TranslationOwner {
    pub plugin: String,
    pub value: Translation,
}
#[derive(Clone, Default)]
pub struct TranslationIndex {
    pub catalogs: BTreeMap<String, BTreeMap<String, TranslationOwner>>,
    pub locales: Vec<String>,
}
pub fn compose_translations(
    mut entries: Vec<TranslationOwner>,
    report: impl Fn(String),
) -> TranslationIndex {
    entries.sort_by(|a, b| a.plugin.cmp(&b.plugin));
    let mut result = TranslationIndex::default();
    let mut locales = BTreeSet::new();
    for entry in entries {
        let Some(locale) = canonical_locale(&entry.value.locale) else {
            report(format!(
                "{}: invalid locale {}",
                entry.plugin, entry.value.locale
            ));
            continue;
        };
        locales.insert(locale.clone());
        let by_locale = result
            .catalogs
            .entry(entry.value.namespace.clone())
            .or_default();
        if let Some(taken) = by_locale.get(&locale) {
            if entry.value.override_catalog == taken.value.override_catalog {
                report(format!(
                    "{} and {} contribute {} in {locale}",
                    taken.plugin, entry.plugin, entry.value.namespace
                ));
            }
            if !entry.value.override_catalog || taken.value.override_catalog {
                continue;
            }
        }
        by_locale.insert(locale, entry);
    }
    result.locales = locales.into_iter().collect();
    result
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessagesReport {
    pub kind: String,
    pub namespace: String,
    pub locale: String,
    pub key: String,
    pub plugin: Option<String>,
    pub used: Option<String>,
}
#[derive(Clone)]
pub struct Language {
    pub locale: String,
    pub dir: String,
    pub chain: Vec<String>,
    pub translations: Arc<TranslationIndex>,
    pub report: Arc<dyn Fn(MessagesReport) + Send + Sync>,
}
pub fn create_language(
    locale: &str,
    translations: Arc<TranslationIndex>,
    report: impl Fn(MessagesReport) + Send + Sync + 'static,
) -> Result<Language> {
    let locale =
        canonical_locale(locale).ok_or_else(|| Error::Invalid("invalid language tag".into()))?;
    Ok(Language {
        dir: direction_of(&locale).into(),
        chain: locale_chain(&locale),
        locale,
        translations,
        report: Arc::new(report),
    })
}
thread_local! {static RENDERING:RefCell<Option<Language>>=const {RefCell::new(None)};}
pub fn current_language() -> Option<Language> {
    RENDERING.with(|state| state.borrow().clone())
}
pub fn in_language<T>(language: Option<Language>, render: impl FnOnce() -> T) -> T {
    struct Restore(Option<Language>);
    impl Drop for Restore {
        fn drop(&mut self) {
            RENDERING.with(|state| *state.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(RENDERING.with(|state| state.replace(language)));
    render()
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageData {
    pub namespace: String,
    pub key: String,
    pub params: Vec<serde_json::Value>,
}
pub struct Messages {
    pub namespace: String,
    own: IndexMap<String, Arc<Catalog>>,
}
impl Messages {
    pub fn define(namespace: &str, catalogs: IndexMap<String, Arc<Catalog>>) -> Result<Self> {
        crate::contracts::check_name(namespace, "A messages namespace")?;
        if catalogs.is_empty() {
            return Err(Error::Invalid("defineMessages needs a catalog".into()));
        }
        let mut own = IndexMap::new();
        for (locale, catalog) in catalogs {
            let canonical = canonical_locale(&locale)
                .ok_or_else(|| Error::Invalid(format!("{locale}: no language tag")))?;
            own.insert(canonical, catalog);
        }
        Ok(Self {
            namespace: namespace.into(),
            own,
        })
    }
    pub fn render(
        &self,
        key: &str,
        args: &[JsonValue],
        language: Option<&Language>,
    ) -> Result<String> {
        let first = self.own.first().map(|(_, c)| c).ok_or(Error::Closed)?;
        let current = current_language();
        let language = language.or(current.as_ref());
        if let Some(language) = language {
            for (index, locale) in language.chain.iter().enumerate() {
                let contributed = language
                    .translations
                    .catalogs
                    .get(&self.namespace)
                    .and_then(|m| m.get(locale));
                let mut candidates = Vec::new();
                if let Some(entry) = contributed {
                    candidates.push((entry.value.messages.clone(), Some(entry.plugin.clone())));
                }
                if let Some(catalog) = self.own.get(locale) {
                    candidates.push((catalog.clone(), None));
                }
                for (catalog, plugin) in candidates {
                    if let Some(value) = catalog.get(key) {
                        match in_language(Some(language.clone()), || value.render(args)) {
                            Ok(text) => {
                                if index > 0 {
                                    (language.report)(self.report(
                                        "fallback",
                                        key,
                                        language,
                                        None,
                                        Some(locale.clone()),
                                    ));
                                }
                                return Ok(text);
                            }
                            Err(error) => {
                                if plugin.is_none() {
                                    return Err(error);
                                }
                                (language.report)(
                                    self.report("failed", key, language, plugin, None),
                                );
                            }
                        }
                    }
                }
            }
            (language.report)(self.report("fallback", key, language, None, None));
        }
        first
            .get(key)
            .ok_or_else(|| Error::Invalid(format!("unknown message {key}")))?
            .render(args)
    }
    fn report(
        &self,
        kind: &str,
        key: &str,
        language: &Language,
        plugin: Option<String>,
        used: Option<String>,
    ) -> MessagesReport {
        MessagesReport {
            kind: kind.into(),
            namespace: self.namespace.clone(),
            locale: language.locale.clone(),
            key: key.into(),
            plugin,
            used,
        }
    }
    pub fn render_data(&self, message: &MessageData, language: Option<&Language>) -> String {
        let known = message.namespace == self.namespace
            && self
                .own
                .first()
                .is_some_and(|(_, c)| c.contains_key(&message.key));
        if !known {
            if let Some(language) = language {
                (language.report)(self.report("unknown", &message.key, language, None, None));
            }
            return message.key.clone();
        }
        self.render(
            &message.key,
            &message
                .params
                .iter()
                .cloned()
                .map(JsonValue::from_serde)
                .collect::<Vec<_>>(),
            language,
        )
        .unwrap_or_else(|_| message.key.clone())
    }
}
struct Inputs {
    pick: Option<String>,
    preferred: Vec<String>,
    offered: Vec<String>,
    stopped: bool,
}
pub struct LanguageFollower {
    inputs: Arc<Mutex<Inputs>>,
    pub locale: NativeSource<String>,
    _offered: NativeObservation<Vec<String>>,
    _pick: NativeObservation<Option<String>>,
}
impl LanguageFollower {
    pub fn follow(
        offered: NativeSource<Vec<String>>,
        pick: NativeSource<Option<String>>,
        preferred: Vec<String>,
        batch: Batch,
        context: &Context,
    ) -> Result<Self> {
        let inputs = Arc::new(Mutex::new(Inputs {
            pick: pick.value(),
            preferred,
            offered: offered.value(),
            stopped: false,
        }));
        let locale = {
            let i = lock(&inputs);
            NativeSource::new(
                batch,
                resolve_locale(i.pick.as_deref(), &i.preferred, &i.offered),
                |a, b| a == b,
            )
        };
        let state = inputs.clone();
        let output = locale.clone();
        let offered = offered.observe(
            context,
            move |call, ctx| {
                if let NativeWatchCall::Value { value } = call {
                    let next = {
                        let mut i = lock(&state);
                        if i.stopped {
                            return Ok(());
                        }
                        i.offered = value;
                        resolve_locale(i.pick.as_deref(), &i.preferred, &i.offered)
                    };
                    output.set(next, ctx)?;
                }
                Ok(())
            },
            |_| {},
        )?;
        let state = inputs.clone();
        let output = locale.clone();
        let pick = pick.observe(
            context,
            move |call, ctx| {
                if let NativeWatchCall::Value { value } = call {
                    let next = {
                        let mut i = lock(&state);
                        if i.stopped {
                            return Ok(());
                        }
                        i.pick = value;
                        resolve_locale(i.pick.as_deref(), &i.preferred, &i.offered)
                    };
                    output.set(next, ctx)?;
                }
                Ok(())
            },
            |_| {},
        )?;
        Ok(Self {
            inputs,
            locale,
            _offered: offered,
            _pick: pick,
        })
    }
    pub fn set_device(&self, preferred: Vec<String>, context: &Context) -> Result<()> {
        let next = {
            let mut i = lock(&self.inputs);
            if i.stopped {
                return Ok(());
            }
            i.preferred = preferred;
            resolve_locale(i.pick.as_deref(), &i.preferred, &i.offered)
        };
        self.locale.set(next, context)
    }
    pub fn stop(&self) {
        lock(&self.inputs).stopped = true;
    }
}
