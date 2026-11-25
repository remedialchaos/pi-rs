use std::{io::Write, path::PathBuf, sync::Arc};

use pi_rs_chord::{
    Error, Result,
    context::Context,
    data::ChordError,
    host::{FacetHost, Members, ObjectSelf, ServeOptions},
    json::JsonValue,
    state::Source,
};
use regex::Regex;
use serde_json::{Value, json};

use super::logs::process_logs;
use crate::api::v1::p2_contracts::lookup;

#[derive(Clone)]
pub(super) struct ServerNames {
    directory: PathBuf,
    separators: Regex,
}

impl ServerNames {
    pub(super) fn new(directory: String) -> Result<Self> {
        Ok(Self {
            directory: directory.into(),
            separators: Regex::new(r"[\p{Cc}\p{Cf}\s]+")
                .map_err(|error| Error::Invalid(error.to_string()))?,
        })
    }

    fn normalize(&self, value: &str) -> Option<String> {
        let line = self.separators.replace_all(value, " ");
        let clipped: String = line.trim().chars().take(63).collect();
        let name = clipped.trim_end();
        (!name.is_empty()).then(|| name.to_owned())
    }

    async fn device(&self) -> Option<String> {
        #[cfg(target_os = "macos")]
        if let Ok(Ok(output)) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::process::Command::new("scutil")
                .args(["--get", "ComputerName"])
                .kill_on_drop(true)
                .output(),
        )
        .await
            && output.status.success()
            && let Some(name) = self.normalize(&String::from_utf8_lossy(&output.stdout))
        {
            return Some(name);
        }
        #[cfg(unix)]
        let host = nix::unistd::gethostname().ok()?;
        #[cfg(not(unix))]
        let host = std::env::var_os("COMPUTERNAME")?;
        let host = host.to_string_lossy();
        let name = if host
            .as_bytes()
            .last_chunk::<6>()
            .is_some_and(|s| s.eq_ignore_ascii_case(b".local"))
        {
            &host[..host.len() - 6]
        } else {
            &host
        };
        self.normalize(name)
    }

    async fn read(&self) -> Result<Option<String>> {
        let contents = match tokio::fs::read(self.directory.join("identity.json")).await {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let json: Value = serde_json::from_slice(&contents)?;
        json["name"]
            .as_str()
            .and_then(|v| self.normalize(v))
            .map(Some)
            .ok_or_else(|| Error::Invalid("identity.json names nothing".into()))
    }

    fn save(&self, name: Option<&str>) -> Result<()> {
        let path = self.directory.join("identity.json");
        if let Some(name) = name {
            let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
            writeln!(file, "{{\n\t\"name\": {}\n}}", serde_json::to_string(name)?)?;
            file.persist(path).map_err(|error| error.error)?;
        } else if let Err(error) = std::fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(error.into());
        }
        Ok(())
    }

    pub(super) fn register(&self, host: &mut FacetHost) -> Result<()> {
        let local = Arc::new(lookup("p2.server-name")?);
        let files = self.clone();
        host.serve(
            local.clone(),
            move |_, _, object| files.members(object),
            ServeOptions::default(),
        )?;
        host.serve(
            Arc::new(lookup("p2.server-name-settings")?),
            move |_, principal, object| {
                if !principal.is_some_and(|who| who["kind"] == "owner") {
                    return Err(
                        ChordError::new("forbidden", "the server's name is the owner's").into(),
                    );
                }
                let source = object.get(local.clone(), JsonValue::Object(Default::default()))?;
                let mut members = Members::variant("owner");
                for field in ["name", "chosen", "device"] {
                    members.state(field, source.member(field)?)?;
                }
                members.method("rename", move |args, ctx| {
                    let source = source.clone();
                    Box::pin(async move { source.call("rename", args, &ctx.context).await })
                })?;
                Ok(members)
            },
            ServeOptions::default(),
        )
    }

    fn members(&self, object: &mut ObjectSelf) -> Result<Members> {
        let chosen = object.writable(JsonValue::Null)?;
        let device = object.writable(JsonValue::Null)?;
        let name = Source::derive(
            object.batch(),
            vec![chosen.clone(), device.clone()],
            |inputs| {
                let [chosen, device] = inputs else {
                    return Err(Error::Invalid("missing server name source".into()));
                };
                Ok(if matches!(chosen.to_json(), JsonValue::Null) {
                    device
                } else {
                    chosen
                }
                .clone())
            },
            |_| {},
        );
        let files = self.clone();
        let initial = chosen.clone();
        let fallback = device.clone();
        object.open(
            move |ctx| {
                Box::pin(async move {
                    fallback.set(
                        JsonValue::from_serde(json!(files.device().await)),
                        &ctx.context,
                    )?;
                    match files.read().await {
                        Ok(value) => initial.set(JsonValue::from_serde(json!(value)), &ctx.context)?,
                        Err(error) => {
                            let message = format!("identity.json is not usable ({error}); the server goes by the device's name");
                            process_logs().notice("server", &message);
                        }
                    }
                    Ok(())
                })
            },
            |_, _| Box::pin(async { Ok(()) }),
        );
        let mut members = Members::new();
        members.state("name", name)?;
        members.state("device", device)?;
        members.state("chosen", chosen.clone())?;
        let (updates, mut pending) = tokio::sync::mpsc::unbounded_channel::<(
            Option<String>,
            tokio::sync::oneshot::Sender<Result<JsonValue>>,
        )>();
        let directory = self.clone();
        // Finish accepted saves in call order even after a client disconnects.
        tokio::spawn(async move {
            while let Some((next, reply)) = pending.recv().await {
                let files = directory.clone();
                let saved = next.clone();
                let result = async {
                    tokio::task::spawn_blocking(move || files.save(saved.as_deref()))
                        .await
                        .map_err(std::io::Error::other)??;
                    chosen.set(JsonValue::from_serde(json!(next)), &Context::background())?;
                    Ok(JsonValue::Null)
                }
                .await;
                let _ = reply.send(result);
            }
        });
        let files = self.clone();
        members.method("rename", move |args, _| {
            let (reply, saved) = tokio::sync::oneshot::channel();
            let queued = args.to_serde().and_then(|args| {
                let name = args["name"].as_str().and_then(|name| files.normalize(name));
                updates.send((name, reply)).map_err(|_| Error::Closed)
            });
            Box::pin(async move {
                queued?;
                saved.await.map_err(|_| Error::Closed)?
            })
        })?;
        Ok(members)
    }
}
