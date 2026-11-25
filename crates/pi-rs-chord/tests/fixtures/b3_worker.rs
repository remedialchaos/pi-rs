use pi_rs_chord::contracts::{Contract, HostKind, Member, read_params, schema as s};
#[cfg(not(test))]
use pi_rs_chord::{
    host::{Members, PluginFacet, ServeOptions, define_facet},
    json::JsonValue,
    node::{
        app::{FileWrite, boot_worker},
        workers::WorkerSession,
    },
};
use std::sync::Arc;

pub fn kind() -> HostKind {
    HostKind::new(
        "project",
        read_params(&[("id".into(), s::str())], "project").unwrap(),
    )
    .unwrap()
}

pub fn contract() -> Arc<Contract> {
    Arc::new(
        Contract::remote(
            "app.project",
            1,
            vec![("id".into(), s::str())],
            [(
                "main".into(),
                [(
                    "who".into(),
                    Member::Method {
                        args: s::unknown(),
                        result: s::unknown(),
                    },
                )]
                .into_iter()
                .collect(),
            )]
            .into_iter()
            .collect(),
            Arc::new(|params| kind().address(params.clone())),
        )
        .unwrap(),
    )
}

#[cfg(not(test))]
async fn worker() -> pi_rs_chord::Result<()> {
    let session = WorkerSession::start(
        tokio::fs::File::open("/dev/stdin").await?,
        tokio::fs::OpenOptions::new()
            .write(true)
            .open("/dev/stdout")
            .await?,
    )
    .await?;
    let seed = session.start["app"].as_str().map(str::to_owned);
    let (ready, seeded) = tokio::sync::watch::channel(false);
    let app = boot_worker(
        session,
        vec![PluginFacet::new(
            "app.project",
            define_facet(move |host| {
                let seeded = seeded.clone();
                host.serve(
                    contract(),
                    move |params, _, _| {
                        let params = params.clone();
                        let seeded = seeded.clone();
                        let mut members = Members::variant("main");
                        members.method("who", move |_, context| {
                            let params = params.clone();
                            let mut seeded = seeded.clone();
                            Box::pin(async move {
                                seeded
                                    .wait_for(|ready| *ready)
                                    .await
                                    .map_err(|_| pi_rs_chord::Error::Closed)?;
                                let mut answer = params.to_serde()?;
                                answer["principal"] = context.principal.unwrap_or_default();
                                Ok(JsonValue::from_serde(answer))
                            })
                        })?;
                        Ok(members)
                    },
                    ServeOptions::default(),
                )
            }),
        )],
    )
    .await?;
    if let Some(seed) = seed
        && app.files.get("test", "artifact", None)?.is_none()
    {
        app.files
            .put(FileWrite::new("test", "artifact"), seed.as_bytes())
            .await?;
    }
    ready.send_replace(true);
    app.run().await
}

#[cfg(not(test))]
fn main() -> pi_rs_chord::Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(worker())
}
