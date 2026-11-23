use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Principal {
    Owner {
        #[serde(flatten)]
        access: OwnerAccess,
    },
    Guest {
        id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        email: Option<String>,
        home: String,
        sessions: BTreeMap<String, GrantMode>,
        revision: u64,
    },
    Worker {
        session: String,
        generation: u64,
    },
    Server,
    Host,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "access", rename_all = "lowercase")]
pub enum OwnerAccess {
    Local,
    Relay,
    Device { device: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GrantMode {
    View,
    Full,
}

impl Principal {
    pub fn is_owner(&self) -> bool {
        matches!(self, Self::Owner { .. })
    }

    pub fn can_view(&self, session: &str) -> bool {
        self.is_owner()
            || matches!(self, Self::Guest { sessions, .. } if sessions.contains_key(session))
    }

    pub fn can_send(&self, session: &str) -> bool {
        self.is_owner()
            || matches!(self, Self::Guest { sessions, .. } if sessions.get(session) == Some(&GrantMode::Full))
    }

    pub fn key(&self) -> String {
        match self {
            Self::Owner {
                access: OwnerAccess::Local,
            } => "owner:local".into(),
            Self::Owner {
                access: OwnerAccess::Relay,
            } => "owner:relay".into(),
            Self::Owner {
                access: OwnerAccess::Device { device },
            } => format!("owner:device:{device}"),
            Self::Guest { id, revision, .. } => format!("guest:{id}:r{revision}"),
            Self::Worker {
                session,
                generation,
            } => format!("worker:{session}:{generation}"),
            Self::Server => "server".into(),
            Self::Host => "host".into(),
        }
    }
}
