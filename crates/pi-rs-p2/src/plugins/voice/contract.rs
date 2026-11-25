//! Shared contracts and voice choice types for the `voice` plugin.

use pi_rs_chord::{Result, contracts::Contract};
use serde::{Deserialize, Serialize};

use crate::api::v1::p2_contracts::lookup;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VoiceChoice {
    pub engine: String,
    pub voice: String,
    pub speed: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

pub fn voice_mode_contract() -> Result<Contract> {
    lookup("p2.voice-mode")
}

pub fn voice_service_store_contract() -> Result<Contract> {
    lookup("p2.voice-service-store")
}

pub fn voice_service_contract() -> Result<Contract> {
    lookup("p2.voice-service")
}
