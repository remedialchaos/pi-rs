use crate::{Error, Result, server_config::Preferences};
use serde::{Deserialize, Serialize};
use std::{str::FromStr, time::Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];
    pub fn title(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
    pub fn load(prefs: &dyn Preferences) -> Self {
        prefs
            .get("appearance")
            .and_then(|p| p.parse().ok())
            .unwrap_or_default()
    }
    pub fn save(self, prefs: &mut dyn Preferences) -> Result<()> {
        prefs.set("appearance", (self != Self::System).then_some(self.name()))
    }
}
impl FromStr for Appearance {
    type Err = Error;
    fn from_str(name: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|p| p.name() == name)
            .ok_or_else(|| Error::Protocol("piHost: invalid preference".into()))
    }
}

/// Only app-driven changes to visible native views fade; web content animates itself.
pub fn fade(app_driven: bool, visible: bool, reduce_motion: bool) -> Option<Duration> {
    (app_driven && visible && !reduce_motion).then_some(Duration::from_millis(250))
}
