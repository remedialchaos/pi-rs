use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use url::Url;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Organization {
    pub id: String,
    pub name: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Online,
    Offline,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Direct {
        origin: Url,
        data_dir: PathBuf,
    },
    Remote {
        organization: String,
        server: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brain {
    pub id: String,
    pub name: String,
    pub organization: Option<Organization>,
    pub shared: bool,
    pub status: Status,
    pub route: Route,
}
impl Brain {
    pub fn label(&self) -> std::borrow::Cow<'_, str> {
        if self.shared {
            format!("Shared · {}", self.name).into()
        } else {
            (&self.name).into()
        }
    }
    pub fn is_direct(&self) -> bool {
        matches!(self.route, Route::Direct { .. })
    }
    pub fn away(&self) -> bool {
        self.status != Status::Online
    }
    pub fn datastore_id(&self) -> Uuid {
        let mut hash = Sha256::new();
        hash.update(b"p2mac:");
        hash.update(self.id.as_bytes());
        let digest = hash.finalize();
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&digest[..16]);
        Uuid::from_bytes(bytes)
    }
}

pub fn url_path(url: &Url) -> &str {
    &url[url::Position::BeforePath..]
}
