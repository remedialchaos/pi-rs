use std::sync::Arc;

use http::{HeaderValue, StatusCode, header};
use percent_encoding::percent_decode_str;
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::Contract,
    host::WeakHost,
    node::http::{
        HttpContribution, HttpError, HttpRequest, HttpResponse, HttpResult, RouteOptions,
    },
};
use serde_json::{Value, json};
use url::Url;

use crate::api::v1::{OwnerAccess, Principal, server::server_of};

use super::pages;

const START: &str = "/mcp/sign-in/";
const CALLBACK: &str = "/mcp/callback";

pub(super) fn routes(host: WeakHost, manager: Arc<Contract>) -> Result<HttpContribution> {
    let mut http = HttpContribution::new("mcp");
    for pattern in ["GET /mcp/sign-in/*", "GET /mcp/callback"] {
        let (host, manager) = (host.clone(), manager.clone());
        let mut options = RouteOptions::public(move |request| {
            Box::pin(dispatch(host.clone(), manager.clone(), request))
        });
        options.page = true;
        http.route(pattern, options)
            .map_err(|error| Error::Invalid(error.to_string()))?;
    }
    Ok(http)
}

async fn dispatch(
    host: WeakHost,
    manager: Arc<Contract>,
    request: HttpRequest,
) -> HttpResult<HttpResponse> {
    let host = host
        .upgrade()
        .ok_or_else(|| HttpError::new(503, "Server closed"))?;
    let access = server_of(&host)?.request_access(&request)?;
    let callback = request.path == CALLBACK;
    let Some(principal @ Principal::Owner { .. }) = access.principal else {
        let status = if access.principal.is_some() {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::UNAUTHORIZED
        };
        return if callback {
            page(
                status,
                "Sign-in not finished",
                "This browser is not signed in to p2 here. Copy this page's full address from the address bar and paste it in p2's MCP panel.",
            )
        } else {
            page(
                status,
                "Sign-in unavailable",
                "Only the p2 owner can sign in to MCP servers.",
            )
        };
    };
    let query = url::form_urlencoded::parse(request.query.trim_start_matches('?').as_bytes())
        .collect::<Vec<_>>();
    let parameter = |name| {
        query
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_ref())
    };
    let context = Context::background();
    let manager = host.get(manager, json!({}).into(), &context).await?;
    if callback {
        let Some(state) = parameter("state") else {
            return page(
                StatusCode::BAD_REQUEST,
                "Sign-in not finished",
                "The authorization response has no state.",
            );
        };
        let mut input = json!({"state":state});
        for field in ["code", "iss"] {
            if let Some(value) = parameter(field) {
                input[field] = json!(value);
            }
        }
        if let Some(error) = parameter("error") {
            input["error"] = json!(parameter("error_description").unwrap_or(error));
        }
        return match manager.call("completeSignIn", input.into(), &context).await {
            Ok(output) => {
                let output: Value = output.to_serde()?;
                page(
                    StatusCode::OK,
                    &format!(
                        "Signed in to {}",
                        output["server"].as_str().unwrap_or_default()
                    ),
                    "You may close this page.",
                )
            }
            Err(error) => page(
                StatusCode::BAD_REQUEST,
                "Sign-in not finished",
                &message(&error),
            ),
        };
    }
    let path = request.path.strip_prefix(START).unwrap_or_default();
    let Ok(name) = percent_decode_str(path).decode_utf8() else {
        return page(
            StatusCode::BAD_REQUEST,
            "Unknown MCP server",
            "The sign-in link is malformed.",
        );
    };
    let local = matches!(
        principal,
        Principal::Owner {
            access: OwnerAccess::Local
        }
    ) && !access.origin.remote;
    let via = parameter("via");
    if !local && !matches!(via, Some("loopback" | "callback")) {
        let result: Value = manager
            .call(
                "probe",
                json!({"origin":access.origin.origin}).into(),
                &context,
            )
            .await?
            .to_serde()?;
        let probe = Url::parse(result["url"].as_str().unwrap_or_default())
            .map_err(|_| HttpError::new(500, "Invalid loopback probe URL"))?;
        return pages::probe(&name, &probe, &request.nonce);
    }
    let redirect = if local || via == Some("loopback") {
        json!({"kind":"loopback"})
    } else {
        json!({"kind":"route","url":format!("{}{CALLBACK}", access.origin.origin)})
    };
    let input = json!({"server":name,"redirect":redirect});
    let result: Value = match manager.call("beginSignIn", input.into(), &context).await {
        Ok(result) => result.to_serde()?,
        Err(error) => {
            return page(
                StatusCode::BAD_GATEWAY,
                &format!("Signing in to {name} failed"),
                &message(&error),
            );
        }
    };
    if result["kind"] == "authorized" {
        return page(
            StatusCode::OK,
            &format!("Signed in to {name}"),
            "You may close this page.",
        );
    }
    let location = result["url"]
        .as_str()
        .ok_or_else(|| HttpError::new(500, "Missing authorization URL"))?;
    let mut response = page(
        StatusCode::FOUND,
        "Sign-in redirected",
        "Continue to the authorization server.",
    )?;
    response
        .headers_mut()
        .insert(header::LOCATION, HeaderValue::from_str(location)?);
    Ok(response)
}

fn page(status: StatusCode, title: &str, message: &str) -> HttpResult<HttpResponse> {
    pages::send(status, pages::result(title, message), "")
}

fn message(error: &Error) -> String {
    match error {
        Error::Chord(error) => error.message.clone(),
        error => error.to_string(),
    }
}
