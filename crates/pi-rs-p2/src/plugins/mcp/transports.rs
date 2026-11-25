//! Open the transport for a placed instance.

use std::sync::Arc;

use indexmap::IndexMap;
use pi_rs_chord::{Error, Result, sync::SyncFuture};
use pi_rs_mcp::{
    AuthProvider, McpTransport, Root, StdioStderr, StdioTransport, StdioTransportOptions,
    StreamableHttpTransport, StreamableHttpTransportOptions,
};

use super::{
    config::expand_host_variables,
    placement::{Connect, OAuthServer},
};

pub type Opened = (Arc<dyn McpTransport>, Vec<Root>);
pub type ConnectRemote = Arc<dyn Fn(Connect) -> SyncFuture<Opened> + Send + Sync>;
pub type Authorize = Arc<dyn Fn(&OAuthServer) -> Result<Arc<dyn AuthProvider>> + Send + Sync>;

pub async fn open(connect: &Connect, remote: &ConnectRemote, auth: &Authorize) -> Result<Opened> {
    match connect {
        Connect::Host {
            command,
            args,
            env,
            cwd,
            roots,
            host_variables,
        } => {
            let transport = StdioTransport::new(StdioTransportOptions {
                command: command.clone(),
                args: args.clone(),
                cwd: Some(cwd.to_string_lossy().into_owned()),
                env: Some(if *host_variables {
                    expand(env)?
                } else {
                    env.clone()
                }),
                stderr: Some(StdioStderr::Pipe),
                ..Default::default()
            });
            Ok((
                transport,
                roots
                    .iter()
                    .map(|uri| Root {
                        uri: uri.clone(),
                        name: None,
                        extra: serde_json::Map::new(),
                    })
                    .collect(),
            ))
        }
        Connect::Http {
            url,
            headers,
            oauth,
        } => {
            let transport = StreamableHttpTransport::new(StreamableHttpTransportOptions {
                url: url.clone(),
                headers: Some(expand(headers)?),
                auth_provider: oauth.as_ref().map(|server| auth(server)).transpose()?,
                ..Default::default()
            })
            .map_err(|error| Error::Invalid(error.to_string()))?;
            Ok((transport, Vec::new()))
        }
        Connect::Remote { .. } => remote(connect.clone()).await,
        Connect::Unplaced { reason } => Err(Error::Invalid(reason.clone())),
    }
}

pub(super) fn expand(values: &IndexMap<String, String>) -> Result<IndexMap<String, String>> {
    values
        .iter()
        .map(|(key, value)| {
            Ok((
                key.clone(),
                expand_host_variables(value).map_err(Error::Invalid)?,
            ))
        })
        .collect()
}
