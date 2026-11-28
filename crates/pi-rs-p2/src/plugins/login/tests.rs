use std::{error::Error as StdError, path::Path, sync::Arc, time::Duration};

use pi_rs_ai::{
    auth::{CredentialStore, FileCredentialStore, LoginOptions},
    models_store::InMemoryModelsStore,
    providers::env_api_key_auth,
    registry::{Models, ModelsOptions, Provider},
};
use pi_rs_chord::{
    context::Context,
    host::{Caller, Held, Members},
    state::Source,
};
use serde_json::{Value, json};

use super::*;
use crate::{
    api::v1::server::install_service,
    hosts::server::{P2Server, ServerOptions, start_server},
};

type TestResult<T = ()> = std::result::Result<T, Box<dyn StdError + Send + Sync>>;
const TIMEOUT: Duration = Duration::from_secs(10);
const PROVIDER: &str = "test-faux";

#[tokio::test]
async fn provider_credentials_are_owner_scoped_persisted_and_deleted() -> TestResult {
    let directory = tempfile::tempdir()?;
    let server = start(directory.path()).await?;
    let context = Context::background();
    let owner = hold(&server, json!({"kind":"owner","access":"local"}), &context)?;
    let other = hold(
        &server,
        json!({"kind":"owner","access":"device","device":"second"}),
        &context,
    )?;
    let guest = hold(
        &server,
        json!({"kind":"guest","id":"reader","name":"Reader","home":"s","sessions":{"s":"full"},"revision":1}),
        &context,
    )?;
    assert!(
        matches!(guest.wait(&context).await, Err(Error::Chord(error)) if error.code == "forbidden")
    );
    owner.wait(&context).await?;
    other.wait(&context).await?;
    assert_eq!(state(&owner, "providers")?[0]["signedIn"], Value::Null);
    let args = json!({"providerId":PROVIDER,"authType":"api_key"});
    let started: Value = owner
        .call("start", args.clone().into(), &context)
        .await?
        .to_serde()?;
    let id = started["attemptId"].clone();
    let prompt = until(&owner, |state| state["prompt"]["type"] == "secret").await?;
    assert_eq!(prompt["attemptId"], id);
    assert_eq!(state(&other, "login")?, Value::Null);
    assert!(other.call("start", args.into(), &context).await.is_err());
    let answer =
        json!({"attemptId":id,"promptId":prompt["prompt"]["id"],"value":"fixture-private-key"});
    assert!(
        other
            .call("answer", answer.clone().into(), &context)
            .await
            .is_err()
    );
    owner.call("answer", answer.into(), &context).await?;
    let done = until(&owner, |state| state["status"] == "success").await?;
    assert!(!done.to_string().contains("fixture-private-key"));
    assert_eq!(done["prompt"], Value::Null);
    assert_eq!(state(&owner, "providers")?[0]["signedIn"], "api_key");
    let identity = tokio::fs::read(directory.path().join("device.json")).await?;
    owner.release();
    other.release();
    server.close().await?;

    let server = start(directory.path()).await?;
    let owner = hold(&server, json!({"kind":"owner","access":"local"}), &context)?;
    let other = hold(
        &server,
        json!({"kind":"owner","access":"device","device":"second"}),
        &context,
    )?;
    owner.wait(&context).await?;
    other.wait(&context).await?;
    assert_eq!(state(&owner, "providers")?[0]["signedIn"], "api_key");
    assert_eq!(state(&owner, "login")?, Value::Null);
    owner
        .call("logout", json!({"providerId":PROVIDER}).into(), &context)
        .await?;
    assert_eq!(state(&owner, "providers")?[0]["signedIn"], Value::Null);
    let started: Value = owner
        .call(
            "start",
            json!({"providerId":PROVIDER,"authType":"api_key"}).into(),
            &context,
        )
        .await?
        .to_serde()?;
    let cancel_id = started["attemptId"].clone();
    until(&owner, |state| state["prompt"]["type"] == "secret").await?;
    let service = server
        .host
        .get(
            Arc::new(lookup("p2.login-service")?),
            json!({}).into(),
            &context,
        )
        .await?;
    other.release();
    assert_eq!(state(&service, "login")?["status"], "working");
    owner
        .call("cancel", json!({"attemptId": cancel_id}).into(), &context)
        .await?;
    until(&service, |state| state["status"] == "cancelled").await?;
    owner.release();
    assert_eq!(
        tokio::fs::read(directory.path().join("device.json")).await?,
        identity
    );
    service.release();
    server.close().await?;
    let creds = FileCredentialStore::new(directory.path().join("auth.json"));
    assert!(
        creds
            .list(&LoginOptions::default().signal)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn login_survives_short_disconnect_and_ends_after_away_timeout() -> TestResult {
    let directory = tempfile::tempdir()?;
    let server = start_with_away(directory.path(), Duration::from_millis(120)).await?;
    let context = Context::background();
    let owner = hold(&server, json!({"kind":"owner","access":"local"}), &context)?;
    owner.wait(&context).await?;
    let service = server
        .host
        .get(
            Arc::new(lookup("p2.login-service")?),
            json!({}).into(),
            &context,
        )
        .await?;

    let started: Value = owner
        .call(
            "start",
            json!({"providerId":PROVIDER,"authType":"api_key"}).into(),
            &context,
        )
        .await?
        .to_serde()?;
    let prompt = until(&owner, |state| state["prompt"]["type"] == "secret").await?;

    // Dropped connection for less than away_timeout keeps the login alive on reconnect.
    owner.release();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(state(&service, "login")?["status"], "working");
    let reconnected = hold(&server, json!({"kind":"owner","access":"local"}), &context)?;
    reconnected.wait(&context).await?;
    tokio::time::sleep(Duration::from_millis(130)).await;
    assert_eq!(state(&service, "login")?["status"], "working");
    reconnected
        .call(
            "answer",
            json!({
                "attemptId": started["attemptId"],
                "promptId": prompt["prompt"]["id"],
                "value": "fixture-reconnected-key"
            })
            .into(),
            &context,
        )
        .await?;
    until(&reconnected, |state| state["status"] == "success").await?;
    reconnected.release();

    // Staying disconnected past away_timeout ends the login with an error.
    let away_owner = hold(&server, json!({"kind":"owner","access":"local"}), &context)?;
    away_owner.wait(&context).await?;
    away_owner
        .call(
            "start",
            json!({"providerId":PROVIDER,"authType":"api_key"}).into(),
            &context,
        )
        .await?;
    until(&away_owner, |state| state["prompt"]["type"] == "secret").await?;
    away_owner.release();
    let ended = until(&service, |state| state["status"] == "error").await?;
    assert!(
        ended["message"]
            .as_str()
            .is_some_and(|m| m.contains("no page of yours was connected")),
        "unexpected message: {ended}"
    );
    service.release();
    server.close().await?;
    Ok(())
}

#[tokio::test]
async fn login_same_owner_replaces_unfinished_attempt_while_other_owner_blocked() -> TestResult {
    let directory = tempfile::tempdir()?;
    let server = start(directory.path()).await?;
    let context = Context::background();
    let owner = hold(&server, json!({"kind":"owner","access":"local"}), &context)?;
    let other = hold(
        &server,
        json!({"kind":"owner","access":"device","device":"second"}),
        &context,
    )?;
    owner.wait(&context).await?;
    other.wait(&context).await?;

    let first: Value = owner
        .call(
            "start",
            json!({"providerId":PROVIDER,"authType":"api_key"}).into(),
            &context,
        )
        .await?
        .to_serde()?;
    until(&owner, |state| state["prompt"]["type"] == "secret").await?;

    // Different owner still cannot start while an attempt is in progress.
    assert!(
        other
            .call(
                "start",
                json!({"providerId":PROVIDER,"authType":"api_key"}).into(),
                &context,
            )
            .await
            .is_err()
    );

    // Same owner starting again replaces their unfinished attempt cleanly.
    let second: Value = owner
        .call(
            "start",
            json!({"providerId":PROVIDER,"authType":"api_key"}).into(),
            &context,
        )
        .await?
        .to_serde()?;
    assert_ne!(first["attemptId"], second["attemptId"]);
    let working = until(&owner, |state| {
        state["status"] == "working" && state["attemptId"] == second["attemptId"]
    })
    .await?;
    assert_eq!(working["attemptId"], second["attemptId"]);

    owner
        .call(
            "cancel",
            json!({"attemptId": second["attemptId"]}).into(),
            &context,
        )
        .await?;
    until(&owner, |state| state["status"] == "cancelled").await?;
    owner.release();
    other.release();
    server.close().await?;
    Ok(())
}

async fn until(held: &Held, matches: impl Fn(&Value) -> bool) -> TestResult<Value> {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let current = state(held, "login")?;
            if matches(&current) {
                return Ok(current);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}

fn state(held: &Held, field: &str) -> TestResult<Value> {
    Ok(held.member(field)?.value()?.to_json().to_serde()?)
}

fn hold(server: &P2Server, principal: Value, context: &Context) -> TestResult<Held> {
    Ok(server.host.hold_as(
        Arc::new(lookup("p2.provider-login")?),
        json!({}).into(),
        Caller {
            principal: Some(principal),
            incarnation: "login-test".into(),
        },
        context,
    )?)
}

async fn start(directory: &Path) -> TestResult<P2Server> {
    start_with_away(directory, flow::AWAY_TIMEOUT).await
}

async fn start_with_away(directory: &Path, away_timeout: Duration) -> TestResult<P2Server> {
    let mut options = ServerOptions::new(directory.into());
    options.port = 0;
    options.fake = true;
    options
        .registry
        .register("login", FacetKind::Server, server_with_away(away_timeout)?)?;
    let directory = directory.to_owned();
    let config = Arc::new(lookup("p2.model-config")?);
    options.registry.register("test-login-models", FacetKind::Server, define_facet(move |host| {
        let directory = directory.clone();
        host.serve(config.clone(), move |_, _, object| {
            let directory = directory.clone();
            let source = Source::new(object.batch(), json!({}).into())?;
            let publish = source.clone();
            let weak = object.host().downgrade();
            object.open(move |_| Box::pin(async move {
                let credentials = Arc::new(FileCredentialStore::new(directory.join("auth.json")));
                let store = Arc::new(InMemoryModelsStore::default());
                let models = Models::new(ModelsOptions {
                    credentials:credentials.clone(), store:store.clone(), ..Default::default()
                });
                let mut provider = Provider::new(PROVIDER, "Fixture provider");
                provider.auth.api_key = Some(env_api_key_auth("Fixture key", ["P2_TEST_UNUSED_FAUX_KEY"]));
                models.set_provider(provider);
                let publish = Arc::new(move || {
                    let (credentials, publish) = (credentials.clone(), publish.clone());
                    let result: SyncFuture<()> = Box::pin(async move {
                        let entries = credentials.list(&LoginOptions::default().signal).await
                            .map_err(|error| Error::Invalid(error.to_string()))?;
                        publish.set(json!({
                            "providers":[{"id":PROVIDER,"name":"Fixture provider","loginMethods":[{"type":"api_key","label":"Fixture key"}]}],
                            "credentials":entries,
                        }).into(), &Context::background())
                    });
                    result
                });
                let resource = Arc::new(LoginResources { directory, models, model_store:store, publish });
                (resource.publish)().await?;
                install_service(&weak.upgrade().ok_or(Error::Closed)?, MODEL_RESOURCES, &resource)?;
                Ok(resource)
            }), |_, _| Box::pin(async { Ok(()) }));
            let mut members = Members::new();
            members.state("snapshot", source)?;
            members.state("summary", Source::new(object.batch(), json!({
                "models":[],"errors":[],"defaultModel":null
            }).into())?)?;
            for method in ["setDefault", "setEnabledModels"] {
                members.method(method, |_, _| Box::pin(async { Err(Error::Invalid("Fixture settings are fixed".into())) }))?;
            }
            Ok(members)
        }, ServeOptions::default())
    }))?;
    Ok(start_server(options).await?)
}
