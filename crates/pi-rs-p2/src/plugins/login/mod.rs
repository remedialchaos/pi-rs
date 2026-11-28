mod flow;
#[cfg(test)]
mod tests;

use std::{path::PathBuf, sync::Arc};

use pi_rs_ai::{
    auth::LoginOptions,
    models_store::ModelsStore,
    registry::{Models, RefreshOptions},
};
use pi_rs_chord::{
    Error, Result,
    data::ChordError,
    delta::Revision,
    host::{Facet, Members, ObjectSelf, ServeOptions, define_facet},
    json::JsonValue,
    state::Source,
    sync::SyncFuture,
    watch::NativeWatchCall,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::v1::{FacetKind, OwnerAccess, PluginRegistry, Principal, p2_contracts::lookup};

use flow::Logins;

pub const MODEL_RESOURCES: &str = "p2.model-resources";

/// The Models plugin publishes the same registry and model cache that back ModelConfig.
pub struct LoginResources {
    pub directory: PathBuf,
    pub models: Models,
    pub model_store: Arc<dyn ModelsStore>,
    pub publish: Arc<dyn Fn() -> SyncFuture<()> + Send + Sync>,
}

impl LoginResources {
    async fn refresh(&self, provider: &str, options: &LoginOptions) -> Result<usize> {
        let report = self
            .models
            .refresh(RefreshOptions {
                providers: Some(vec![provider.into()]),
                allow_network: true,
                signal: options.signal.clone(),
            })
            .await;
        (self.publish)().await?;
        if options.signal.is_cancelled() {
            return Err(ChordError::new("aborted", "Model refresh cancelled").into());
        }
        if !report.errors.is_empty() {
            return Err(Error::Invalid("Could not refresh provider models".into()));
        }
        Ok(self.models.get_models(Some(provider)).len())
    }
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register("login", FacetKind::Server, server()?)
}

fn server() -> Result<Facet> {
    server_with_away(flow::AWAY_TIMEOUT)
}

fn server_with_away(away_timeout: std::time::Duration) -> Result<Facet> {
    let service = Arc::new(lookup("p2.login-service")?);
    let remote = Arc::new(lookup("p2.provider-login")?);
    let config = Arc::new(lookup("p2.model-config")?);
    Ok(define_facet(move |host| {
        let references = host.host().references("p2.provider-login");
        host.serve(
            service.clone(),
            move |_, _, object| {
                let state = Source::new(object.batch(), json!({"login":null,"owner":null}).into())?;
                let host = object.host().downgrade();
                let publish = state.clone();
                let logins = object.open(
                    move |_| {
                        Box::pin(async move { Ok(Logins::with_away(host, publish, away_timeout)) })
                    },
                    |logins, _| {
                        Box::pin(async move {
                            logins.close().await;
                            Ok(())
                        })
                    },
                );
                let follow = logins.clone();
                let references = references.clone();
                object.on_live(move |ctx| {
                    let (follow, references) = (follow.clone(), references.clone());
                    Box::pin(async move {
                        let logins = follow.value()?;
                        let changed = logins.clone();
                        let observation = references.observe(
                            &ctx.context,
                            move |event, _| {
                                if let NativeWatchCall::Value { value } = event {
                                    changed.sync_presence(&value);
                                }
                                Ok(())
                            },
                            |_| {},
                        )?;
                        logins.sync_presence(&observation.value);
                        std::future::pending::<()>().await;
                        Ok(())
                    })
                });
                let mut members = Members::new();
                for key in ["login", "owner"] {
                    members.state(
                        key,
                        Source::derive(
                            object.batch(),
                            vec![state.clone()],
                            move |values| {
                                let fields: Value = values[0].to_json().to_serde()?;
                                Ok(Revision::from_json(fields[key].clone().into())?)
                            },
                            |_| {},
                        ),
                    )?;
                }
                for name in ["start", "answer", "cancel", "dismiss", "logout", "refresh"] {
                    let logins = logins.clone();
                    members.method(name, move |args, _| {
                        let handle = logins.clone();
                        Box::pin(async move {
                            handle
                                .value()?
                                .call(name, args.to_serde()?)
                                .await
                                .map(JsonValue::from)
                        })
                    })?;
                }
                Ok(members)
            },
            ServeOptions::default(),
        )?;
        let (config, service) = (config.clone(), service.clone());
        host.serve(
            remote.clone(),
            move |_, principal, object| provider_members(object, principal, &config, &service),
            ServeOptions::default(),
        )
    }))
}

fn provider_members(
    object: &mut ObjectSelf,
    principal: Option<&Value>,
    config: &Arc<pi_rs_chord::contracts::Contract>,
    service: &Arc<pi_rs_chord::contracts::Contract>,
) -> Result<Members> {
    let principal = principal
        .map(Principal::deserialize)
        .transpose()?
        .filter(Principal::is_owner)
        .ok_or_else(|| {
            ChordError::new("forbidden", "Provider login is only available to owners")
        })?;
    let remote = !matches!(
        principal,
        Principal::Owner {
            access: OwnerAccess::Local
        }
    );
    let owner = principal.key();
    let config = object.get(config.clone(), json!({}).into())?;
    let service = object.get(service.clone(), json!({}).into())?;
    let mut members = Members::variant("owner");
    let viewer = owner.clone();
    members.state(
        "login",
        Source::derive(
            object.batch(),
            vec![service.member("login")?, service.member("owner")?],
            move |values| {
                let started: Value = values[1].to_json().to_serde()?;
                if started == viewer {
                    Ok(values[0].clone())
                } else {
                    Ok(Revision::from_json(JsonValue::Null)?)
                }
            },
            |_| {},
        ),
    )?;
    members.state("providers", Source::derive(
        object.batch(),
        vec![config.member("snapshot")?],
        move |values| {
            let snapshot: Value = values[0].to_json().to_serde()?;
            let providers = snapshot["providers"].as_array().into_iter().flatten()
                .filter(|provider| provider["loginMethods"].as_array().is_some_and(|methods| !methods.is_empty()))
                .map(|provider| {
                    let stored = snapshot["credentials"].as_array().into_iter().flatten()
                        .find(|credential| credential["providerId"] == provider["id"]);
                    json!({"id":provider["id"], "name":provider["name"], "loginMethods":provider["loginMethods"],
                        "signedIn":stored.map(|item| &item["type"])})
                }).collect::<Vec<_>>();
            Ok(Revision::from_json(json!(providers).into())?)
        },
        |_| {},
    ))?;
    for name in ["start", "answer", "cancel", "dismiss", "logout", "refresh"] {
        let (service, owner) = (service.clone(), owner.clone());
        members.method(name, move |args, ctx| {
            let (service, owner) = (service.clone(), owner.clone());
            Box::pin(async move {
                let mut args: Value = args.to_serde()?;
                if matches!(name, "start" | "answer" | "cancel" | "dismiss") {
                    args["owner"] = json!(owner);
                }
                if name == "start" {
                    args["remote"] = json!(remote);
                }
                service.call(name, args.into(), &ctx.context).await
            })
        })?;
    }
    Ok(members)
}
