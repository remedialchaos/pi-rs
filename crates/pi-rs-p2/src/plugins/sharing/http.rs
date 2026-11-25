use super::server::{call, catalog_row};
use crate::{api::v1::Principal, runtime::server::server_of};
use http::{HeaderValue, Method, StatusCode, header};
use pi_rs_chord::{
    context::Context,
    contracts::Contract,
    host::WeakHost,
    node::http::{
        HttpContribution, HttpError, HttpRequest, HttpResponse, HttpResult, HttpWrap, RouteOptions,
        WrapPhase, html_response, json_response, text_response,
    },
};
use serde_json::{Value, json};
use std::sync::Arc;

const JOIN_SCRIPT: &str = "document.getElementById('join').onsubmit=async(e)=>{e.preventDefault();const status=document.getElementById('status');try{const r=await fetch('/join',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({token:location.pathname.split('/').pop(),name:document.getElementById('name').value})});const v=await r.json();if(!r.ok)throw new Error(v.error);location.replace('/s/'+v.session);}catch(e){status.className='error';status.textContent=e.message;}};";

fn cookie(request: &HttpRequest, port: u16) -> Option<&str> {
    let key = format!("p2_guest_{port}");
    request.header("cookie")?.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        (name == key).then_some(value)
    })
}

fn page(
    request: &HttpRequest,
    status: StatusCode,
    title: &str,
    heading: &str,
    body: &str,
    script: Option<&str>,
) -> HttpResult<HttpResponse> {
    let heading = heading
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    let nonce = &request.nonce;
    let script = script.map_or_else(String::new, |script| {
        format!("<script nonce=\"{nonce}\">{script}</script>")
    });
    let mut response = html_response(format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head><body><main><h1>{heading}</h1>{body}</main>{script}</body></html>"
    ))?;
    *response.status_mut() = status;
    response.headers_mut().insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_str(&format!(
        "default-src 'none'; script-src 'nonce-{nonce}'; style-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    ))?);
    if request.method == Method::HEAD {
        *response.body_mut() = Default::default();
    }
    Ok(response)
}

async fn dispatch(
    weak: WeakHost,
    shares: Arc<Contract>,
    request: HttpRequest,
) -> HttpResult<HttpResponse> {
    let host = weak
        .upgrade()
        .ok_or_else(|| HttpError::new(503, "server closed"))?;
    let natives = server_of(&host)?;
    let access = natives.request_access(&request)?;
    if access.origin.relay_user.is_some() {
        return text_response(
            403,
            "Share links do not work through Radius. Ask the owner to share the session with your email.\n",
        );
    }
    let context = Context::background();
    if request.method == Method::POST {
        if request.header("origin") != Some(access.origin.origin.as_str()) {
            let mut response = json_response(&json!({"error":"Invalid origin"}))?;
            *response.status_mut() = StatusCode::FORBIDDEN;
            return Ok(response);
        }
        let result = match request.json(2048).await {
            Ok(mut input) if input["token"].is_string() && input["name"].is_string() => {
                if let Some(value) = cookie(&request, access.origin.port) {
                    input["cookie"] = json!(value);
                } else if let Some(input) = input.as_object_mut() {
                    input.remove("cookie");
                }
                call(&host, shares, "join", input, &context).await
            }
            _ => Err(super::store_data::join_error("Invalid join")),
        };
        let joined = match result {
            Ok(joined) => joined,
            Err(error) => {
                let message = match &error {
                    pi_rs_chord::Error::Chord(error) if error.code == "invalid" => {
                        error.message.as_str()
                    }
                    _ => "Could not join this session.",
                };
                let mut response = json_response(&json!({"error":message}))?;
                *response.status_mut() = StatusCode::BAD_REQUEST;
                return Ok(response);
            }
        };
        let mut response = json_response(&json!({"session":joined["session"]}))?;
        if let Some(cookie) = joined["cookie"].as_str() {
            let secure = if access.origin.remote { "; Secure" } else { "" };
            response.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&format!(
                "p2_guest_{}={cookie}; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000{secure}", access.origin.port
            ))?);
        }
        return Ok(response);
    }
    let token = request.path.strip_prefix("/join/").unwrap_or_default();
    let link = call(&host, shares, "active", json!({"token":token}), &context).await?;
    let Some(session) = link["session"].as_str() else {
        return page(
            &request,
            StatusCode::GONE,
            "Share ended",
            "This share has ended",
            "<p>The link was reset or the session was archived. Ask the owner for a new one.</p>",
            None,
        );
    };
    let already_joined = match access.principal {
        Some(Principal::Guest { sessions, .. }) => sessions
            .get(session)
            .is_some_and(|mode| json!(mode) == link["mode"]),
        principal => principal.is_some_and(|principal| principal.is_owner()),
    };
    if already_joined {
        let mut response = text_response(303, "")?;
        response.headers_mut().insert(
            header::LOCATION,
            HeaderValue::from_str(&format!("/s/{session}"))?,
        );
        return Ok(response);
    }
    let row = catalog_row(&host, session, &context)
        .await?
        .unwrap_or(Value::Null);
    let title = row["title"].as_str().unwrap_or("Join shared session");
    let access = if link["mode"] == "full" {
        "You can read, send messages and uploads, and stop runs. This lets you make the agent run commands on the host."
    } else {
        "You can read along, but not send messages."
    };
    let content = format!(
        "<p>A session has been shared with you. {access} Enter your name to join; names are not verified.</p><form id=\"join\"><label for=\"name\">Your name</label><input id=\"name\" maxlength=\"40\" required autocomplete=\"nickname\"><button>Join session</button></form><p id=\"status\" role=\"status\"></p>"
    );
    page(
        &request,
        StatusCode::OK,
        "Join shared session",
        title,
        &content,
        Some(JOIN_SCRIPT),
    )
}

pub(super) fn routes(
    host: WeakHost,
    shares: Arc<Contract>,
) -> pi_rs_chord::Result<HttpContribution> {
    let mut http = HttpContribution::new("sharing");
    let join_host = host.clone();
    let join_shares = shares.clone();
    let mut page = HttpWrap::new(
        "sharing.join-page",
        WrapPhase::Transform,
        move |request, _| Box::pin(dispatch(join_host.clone(), join_shares.clone(), request)),
    );
    page.route = Some("GET /join/*".into());
    page.all = true;
    http.wraps.push(page);
    let join_host = host.clone();
    let join_shares = shares.clone();
    http.route(
        "POST /join",
        RouteOptions::public(move |request| {
            Box::pin(dispatch(join_host.clone(), join_shares.clone(), request))
        }),
    )
    .map_err(|err| pi_rs_chord::Error::Invalid(err.to_string()))?;
    http.authenticators.push(Arc::new(move |request| {
        let host = host.clone();
        let shares = shares.clone();
        Box::pin(async move {
            let host = host
                .upgrade()
                .ok_or_else(|| HttpError::new(503, "server closed"))?;
            if matches!(
                request.provenance,
                pi_rs_chord::node::http::Provenance::Local
            ) && crate::runtime::http::forwarded(&request.headers)
            {
                return Ok(None);
            }
            let access = server_of(&host)?.request_access(&request)?;
            let (method, args) = if let Some(user) = access.origin.relay_user {
                ("authenticateEmail", json!({"email":user.email}))
            } else if let Some(value) = cookie(&request, access.origin.port) {
                ("authenticate", json!({"cookie":value}))
            } else {
                return Ok(None);
            };
            let principal = call(&host, shares, method, args, &Context::background()).await?;
            Ok((!principal.is_null()).then_some(principal))
        })
    }));
    Ok(http)
}
