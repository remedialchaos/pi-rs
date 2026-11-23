//! `devices` built-in plugin: installation-owned owner authentication, local challenge-proof
//! sign-in, device cookie sessions, logout/rotation, and `p2.device-access` / `p2.devices`.

pub mod owner_access;

use std::sync::{Arc, Mutex};

use axum::body::Body;
use http::{HeaderValue, Response, StatusCode, header};
use pi_rs_chord::{
    Error, Result,
    data::ChordError,
    host::{Members, ServeOptions, define_facet},
    json::JsonValue,
    node::http::{HttpContribution, HttpRequest, HttpResponse, Provenance, RouteOptions},
};
use serde::Deserialize;
use serde_json::json;

use crate::{
    api::v1::{FacetKind, PluginRegistry, Principal, p2_contracts::lookup},
    runtime::{
        http::same_origin,
        local_owner::{LOCAL_CHALLENGE_PATH, LOCAL_CLAIM_PATH},
        principals::OwnerAccess as PrincipalOwnerAccess,
    },
};

use owner_access::{OwnerAccess, OwnerSession, OwnerSessionKind};

const MAX_BODY_BYTES: usize = 1024;

fn request_port(request: &HttpRequest) -> u16 {
    url::Url::parse(&request.origin)
        .ok()
        .and_then(|parsed| parsed.port_or_known_default())
        .unwrap_or(80)
}

fn is_remote(request: &HttpRequest) -> bool {
    !matches!(request.provenance, Provenance::Local)
}

fn same_origin_post(request: &HttpRequest) -> bool {
    same_origin(&request.headers, &request.origin)
        && request.header("sec-fetch-site") != Some("cross-site")
}

fn text_status(status: StatusCode, body: &'static str) -> HttpResponse {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response
}

fn json_status(
    status: StatusCode,
    value: &serde_json::Value,
    set_cookie: Option<String>,
) -> HttpResponse {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    if let Some(cookie) = set_cookie
        && let Ok(val) = HeaderValue::from_str(&cookie)
    {
        response.headers_mut().insert(header::SET_COOKIE, val);
    }
    response
}

fn authenticate_request(access: &OwnerAccess, request: &HttpRequest) -> Option<serde_json::Value> {
    let cookie_header = request.header("cookie")?;
    let port = request_port(request);
    let session = access.session_from_cookie_header(cookie_header, port)?;
    match session.kind {
        OwnerSessionKind::Device => Some(json!({
            "kind": "owner",
            "access": "device",
            "device": session.id,
        })),
        OwnerSessionKind::Local if !is_remote(request) => Some(json!({
            "kind": "owner",
            "access": "local",
        })),
        OwnerSessionKind::Local => None,
    }
}

fn session_of(access: &OwnerAccess, request: &HttpRequest) -> Option<OwnerSession> {
    let cookie_header = request.header("cookie")?;
    access.session_from_cookie_header(cookie_header, request_port(request))
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let device_access_contract = Arc::new(lookup("p2.device-access")?);
    let devices_contract = Arc::new(lookup("p2.devices")?);
    let server_contract = Arc::new(lookup("p2.server")?);

    registry.register(
        "devices",
        FacetKind::Server,
        define_facet(move |host| {
            let shared_access: Arc<Mutex<Option<OwnerAccess>>> = Arc::new(Mutex::new(None));
            let shared_server: Arc<Mutex<Option<pi_rs_chord::host::Held>>> =
                Arc::new(Mutex::new(None));

            let mut http = HttpContribution::new("devices");
            let auth_access = shared_access.clone();
            http.authenticators.push(Arc::new(move |request| {
                let auth_access = auth_access.clone();
                Box::pin(async move {
                    let access = auth_access
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .clone();
                    Ok(access.and_then(|opened| authenticate_request(&opened, &request)))
                })
            }));

            http.route(
                "GET /owner/check",
                RouteOptions::public(|request| {
                    Box::pin(async move {
                        let is_owner = request
                            .principal
                            .as_ref()
                            .and_then(|v| Principal::deserialize(v).ok())
                            .is_some_and(|p| p.is_owner());
                        if !is_owner {
                            return Ok(text_status(StatusCode::UNAUTHORIZED, "Sign in required"));
                        }
                        Ok(json_status(StatusCode::OK, &json!({ "owner": true }), None))
                    })
                }),
            )
            .map_err(|err| Error::Invalid(err.to_string()))?;

            let token_access = shared_access.clone();
            http.route(
                "GET /owner-token",
                RouteOptions::public(move |request| {
                    let token_access = token_access.clone();
                    Box::pin(async move {
                        let principal = request
                            .principal
                            .as_ref()
                            .and_then(|v| Principal::deserialize(v).ok());
                        let site = request.header("sec-fetch-site");
                        let local_owner = matches!(
                            principal,
                            Some(Principal::Owner {
                                access: PrincipalOwnerAccess::Local
                            })
                        );
                        if !local_owner
                            || is_remote(&request)
                            || (site.is_some() && site != Some("same-origin"))
                        {
                            return Ok(text_status(StatusCode::FORBIDDEN, "Forbidden"));
                        }
                        let Some(access) = token_access
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .clone()
                        else {
                            return Ok(text_status(
                                StatusCode::SERVICE_UNAVAILABLE,
                                "Owner access unavailable",
                            ));
                        };
                        Ok(json_status(
                            StatusCode::OK,
                            &json!({ "token": access.issue_token() }),
                            None,
                        ))
                    })
                }),
            )
            .map_err(|err| Error::Invalid(err.to_string()))?;

            let claim_access = shared_access.clone();
            http.route(
                "POST /owner/claim",
                RouteOptions::public(move |request| {
                    let claim_access = claim_access.clone();
                    Box::pin(async move {
                        if !same_origin_post(&request) {
                            return Ok(text_status(StatusCode::FORBIDDEN, "Invalid origin"));
                        }
                        let bytes = match request.body(MAX_BODY_BYTES).await {
                            Ok(bytes) => bytes,
                            Err(err) if err.status == 413 => {
                                return Ok(text_status(StatusCode::PAYLOAD_TOO_LARGE, "Too large"));
                            }
                            Err(_) => {
                                return Ok(text_status(
                                    StatusCode::BAD_REQUEST,
                                    "Invalid sign-in request",
                                ));
                            }
                        };
                        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                            return Ok(text_status(
                                StatusCode::BAD_REQUEST,
                                "Invalid sign-in request",
                            ));
                        };
                        let Some(access) = claim_access
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .clone()
                        else {
                            return Ok(text_status(
                                StatusCode::SERVICE_UNAVAILABLE,
                                "Owner access unavailable",
                            ));
                        };
                        let token = value.get("token").and_then(|v| v.as_str()).unwrap_or("");
                        if !access.claim(token) {
                            return Ok(text_status(StatusCode::FORBIDDEN, "Invalid one-time token"));
                        }
                        let port = request_port(&request);
                        let cookie = if is_remote(&request) {
                            access.cookie(port, true)
                        } else {
                            access.local_cookie(port, None)
                        };
                        Ok(json_status(
                            StatusCode::OK,
                            &json!({ "ok": true }),
                            Some(cookie),
                        ))
                    })
                }),
            )
            .map_err(|err| Error::Invalid(err.to_string()))?;

            let challenge_access = shared_access.clone();
            http.route(
                &format!("POST {LOCAL_CHALLENGE_PATH}"),
                RouteOptions::public(move |request| {
                    let challenge_access = challenge_access.clone();
                    Box::pin(async move {
                        if is_remote(&request) {
                            return Ok(text_status(StatusCode::FORBIDDEN, "Forbidden"));
                        }
                        if !same_origin_post(&request) {
                            return Ok(text_status(StatusCode::FORBIDDEN, "Invalid origin"));
                        }
                        let Some(access) = challenge_access
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .clone()
                        else {
                            return Ok(text_status(
                                StatusCode::SERVICE_UNAVAILABLE,
                                "Owner access unavailable",
                            ));
                        };
                        Ok(json_status(
                            StatusCode::OK,
                            &json!({ "challenge": access.issue_challenge() }),
                            None,
                        ))
                    })
                }),
            )
            .map_err(|err| Error::Invalid(err.to_string()))?;

            let local_claim_access = shared_access.clone();
            http.route(
                &format!("POST {LOCAL_CLAIM_PATH}"),
                RouteOptions::public(move |request| {
                    let local_claim_access = local_claim_access.clone();
                    Box::pin(async move {
                        if is_remote(&request) {
                            return Ok(text_status(StatusCode::FORBIDDEN, "Forbidden"));
                        }
                        if !same_origin_post(&request) {
                            return Ok(text_status(StatusCode::FORBIDDEN, "Invalid origin"));
                        }
                        let bytes = match request.body(MAX_BODY_BYTES).await {
                            Ok(bytes) => bytes,
                            Err(err) if err.status == 413 => {
                                return Ok(text_status(StatusCode::PAYLOAD_TOO_LARGE, "Too large"));
                            }
                            Err(_) => {
                                return Ok(text_status(
                                    StatusCode::BAD_REQUEST,
                                    "Invalid sign-in request",
                                ));
                            }
                        };
                        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                            return Ok(text_status(
                                StatusCode::BAD_REQUEST,
                                "Invalid sign-in request",
                            ));
                        };
                        let Some(access) = local_claim_access
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .clone()
                        else {
                            return Ok(text_status(
                                StatusCode::SERVICE_UNAVAILABLE,
                                "Owner access unavailable",
                            ));
                        };
                        let challenge = value
                            .get("challenge")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let proof = value.get("proof").and_then(|v| v.as_str()).unwrap_or("");
                        let port = request_port(&request);
                        if !access.prove_local(challenge, proof, port) {
                            return Ok(text_status(StatusCode::FORBIDDEN, "Invalid local sign-in"));
                        }
                        let cookie = access.local_cookie(port, None);
                        Ok(json_status(
                            StatusCode::OK,
                            &json!({ "ok": true }),
                            Some(cookie),
                        ))
                    })
                }),
            )
            .map_err(|err| Error::Invalid(err.to_string()))?;

            for (path, all) in [("POST /owner/logout", false), ("POST /owner/logout-all", true)] {
                let logout_access = shared_access.clone();
                let logout_server = shared_server.clone();
                http.route(
                    path,
                    RouteOptions::public(move |request| {
                        let logout_access = logout_access.clone();
                        let logout_server = logout_server.clone();
                        Box::pin(async move {
                            if !same_origin_post(&request) {
                                return Ok(text_status(StatusCode::FORBIDDEN, "Invalid origin"));
                            }
                            let is_owner = request
                                .principal
                                .as_ref()
                                .and_then(|v| Principal::deserialize(v).ok())
                                .is_some_and(|p| p.is_owner());
                            if all && !is_owner {
                                return Ok(text_status(StatusCode::FORBIDDEN, "Forbidden"));
                            }
                            let Some(access) = logout_access
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .clone()
                            else {
                                return Ok(text_status(
                                    StatusCode::SERVICE_UNAVAILABLE,
                                    "Owner access unavailable",
                                ));
                            };
                            let session = session_of(&access, &request);
                            let op_result = if all {
                                access.rotate()
                            } else if let Some(ref current) = session {
                                access.logout(current)
                            } else {
                                Ok(())
                            };
                            let server = logout_server
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .clone();
                            if let Some(server) = server {
                                let ctx = pi_rs_chord::context::Context::background();
                                if all {
                                    let _ = server
                                        .call(
                                            "revokeOwners",
                                            JsonValue::from_serde(json!({})),
                                            &ctx,
                                        )
                                        .await;
                                } else if let Some(OwnerSession {
                                    kind: OwnerSessionKind::Device,
                                    id,
                                    ..
                                }) = session
                                {
                                    let _ = server
                                        .call(
                                            "revokeOwners",
                                            JsonValue::from_serde(json!({ "device": id })),
                                            &ctx,
                                        )
                                        .await;
                                }
                            }
                            if op_result.is_err() {
                                return Ok(text_status(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    "Internal Server Error",
                                ));
                            }
                            let cookie =
                                access.clear_cookie(request_port(&request), is_remote(&request));
                            Ok(json_status(
                                StatusCode::OK,
                                &json!({ "ok": true }),
                                Some(cookie),
                            ))
                        })
                    }),
                )
                .map_err(|err| Error::Invalid(err.to_string()))?;
            }

            host.contribute_http(http)?;

            let access_slot = shared_access.clone();
            let server_slot = shared_server.clone();
            let server_ref = server_contract.clone();
            let access_for_devices = device_access_contract.clone();
            host.serve(
                device_access_contract.clone(),
                move |_, _, object| {
                    let server = object.get(server_ref.clone(), JsonValue::from_serde(json!({})))?;
                    let open_slot = access_slot.clone();
                    let open_server_slot = server_slot.clone();
                    let close_slot = access_slot.clone();
                    let close_server_slot = server_slot.clone();
                    let _opened = object.open(
                        move |_ctx| {
                            let server = server.clone();
                            let open_slot = open_slot.clone();
                            let open_server_slot = open_server_slot.clone();
                            Box::pin(async move {
                                let info = server.member("info")?.value()?.to_json().to_serde()?;
                                let data_dir = info["dataDir"]
                                    .as_str()
                                    .ok_or_else(|| Error::Invalid("missing server dataDir".into()))?;
                                let opened = OwnerAccess::open(data_dir)
                                    .map_err(|err| Error::Invalid(err.to_string()))?;
                                *open_slot
                                    .lock()
                                    .unwrap_or_else(|poison| poison.into_inner()) =
                                    Some(opened.clone());
                                *open_server_slot
                                    .lock()
                                    .unwrap_or_else(|poison| poison.into_inner()) = Some(server);
                                Ok(opened)
                            })
                        },
                        move |opened, _| {
                            let close_slot = close_slot.clone();
                            let close_server_slot = close_server_slot.clone();
                            Box::pin(async move {
                                opened.close();
                                *close_slot
                                    .lock()
                                    .unwrap_or_else(|poison| poison.into_inner()) = None;
                                *close_server_slot
                                    .lock()
                                    .unwrap_or_else(|poison| poison.into_inner()) = None;
                                Ok(())
                            })
                        },
                    );
                    Ok(Members::new())
                },
                ServeOptions::default(),
            )?;

            let devices_access = shared_access.clone();
            host.serve(
                devices_contract.clone(),
                move |_, principal, object| {
                    let parsed = principal
                        .and_then(|value| Principal::deserialize(value).ok())
                        .ok_or_else(|| ChordError::new("forbidden", "Devices is owner-only"))?;
                    let local = match parsed {
                        Principal::Owner {
                            access: PrincipalOwnerAccess::Local,
                        } => true,
                        Principal::Owner { .. } => false,
                        _ => return Err(ChordError::new("forbidden", "Devices is owner-only").into()),
                    };
                    let _dep =
                        object.get(access_for_devices.clone(), JsonValue::from_serde(json!({})))?;
                    let mut members = Members::variant("owner");
                    members.state("canMint", object.writable(JsonValue::from_serde(json!(local)))?)?;
                    let token_slot = devices_access.clone();
                    members.method("token", move |_, _| {
                        let token_slot = token_slot.clone();
                        Box::pin(async move {
                            if !local {
                                return Err(ChordError::new(
                                    "forbidden",
                                    "Generate owner links on the host with npm run owner-link",
                                )
                                .into());
                            }
                            let access = token_slot
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .clone()
                                .ok_or_else(|| {
                                    ChordError::new("not_ready", "The owner access is not open")
                                })?;
                            Ok(JsonValue::from_serde(json!({ "token": access.issue_token() })))
                        })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )
}
