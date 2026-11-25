use std::sync::Arc;

use base64::{Engine, engine::general_purpose::STANDARD};
use http::{HeaderValue, Method, header};
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::Contract,
    data::ChordError,
    host::{Facet, FacetHost, Members, ServeOptions, WeakHost, define_facet},
    node::http::{
        HttpContribution, HttpError, HttpRequest, HttpResponse, HttpResult, RouteOptions,
        json_response, text_response,
    },
};
use pi_rs_durable::{EntryQuery, harness::Conversation};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::v1::{Principal, p2_contracts::lookup, server::cross_site_read, worker::worker_of};

const PATH: &str = "/entry-image";
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

#[derive(Deserialize)]
pub(super) struct Place {
    entry: u64,
    message: usize,
    part: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Image {
    mime_type: String,
    data: String,
}

pub(super) fn server() -> Result<Facet> {
    let images = Arc::new(lookup("p2.chat-images")?);
    Ok(define_facet(move |host| {
        let images = images.clone();
        let weak = host.host().downgrade();
        let mut http = HttpContribution::new("p2.chat-image-routes");
        http.route(
            &format!("GET {PATH}"),
            RouteOptions::new(move |request| {
                Box::pin(serve(weak.clone(), images.clone(), request))
            }),
        )
        .map_err(|error| Error::Invalid(error.to_string()))?;
        host.contribute_http(http)
    }))
}

pub(super) fn worker(host: &mut FacetHost) -> Result<()> {
    let images = Arc::new(lookup("p2.chat-images")?);
    let worker = worker_of(&host.host())?;
    host.serve(
        images.clone(),
        move |params, principal, _| {
            if principal.map(Principal::deserialize).transpose()? != Some(Principal::Server) {
                return Err(ChordError::new("forbidden", "server only").into());
            }
            let conversation = params.to_serde()?["conversation"]
                .as_u64()
                .ok_or_else(|| Error::Invalid("Invalid conversation ID".into()))?;
            let worker = worker.clone();
            let mut members = Members::variant("server");
            members.method("image", move |args, ctx| {
                let worker = worker.clone();
                Box::pin(async move {
                    let place = serde_json::from_value(args.to_serde()?)?;
                    let harness = worker.harness()?;
                    let conversation = harness
                        .durable
                        .conversation(conversation, &ctx.context)
                        .await
                        .map_err(|error| Error::Invalid(error.to_string()))?
                        .ok_or_else(|| Error::Invalid("Unknown conversation".into()))?;
                    Ok(image_at(&conversation, place, &ctx.context).await?.into())
                })
            })?;
            Ok(members)
        },
        ServeOptions::default(),
    )
}

async fn serve(
    host: WeakHost,
    images: Arc<Contract>,
    request: HttpRequest,
) -> HttpResult<HttpResponse> {
    let query = form_urlencoded::parse(request.query.as_bytes());
    let field = |name: &str| {
        query
            .clone()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    };
    let session = field("session").unwrap_or_default();
    let principal = request
        .principal
        .as_ref()
        .map(Principal::deserialize)
        .transpose()?;
    if !principal.is_some_and(|principal| principal.can_view(&session))
        || cross_site_read(&request.method, &request.headers)
    {
        return text_response(403, "Forbidden");
    }
    let integer = |name: &str, minimum: f64| {
        let field = field(name);
        let raw = field.as_deref().unwrap_or("").trim();
        let value = if raw.is_empty() {
            0.
        } else {
            raw.parse::<f64>().ok()?
        };
        ((minimum..=MAX_SAFE_INTEGER).contains(&value) && value.fract() == 0.)
            .then_some(value as u64)
    };
    let (Some(conversation), Some(entry), Some(message), Some(part)) = (
        integer("conversation", 1.),
        integer("entry", 1.),
        integer("message", 0.),
        integer("part", 0.),
    ) else {
        let mut response = json_response(&json!({"error":"Invalid image location"}))?;
        *response.status_mut() = http::StatusCode::BAD_REQUEST;
        return Ok(response);
    };
    let host = host
        .upgrade()
        .ok_or_else(|| HttpError::new(503, "server closed"))?;
    let context = Context::background();
    let result = async {
        let held = host
            .get(
                images,
                json!({"session":session.as_ref(),"conversation":conversation}).into(),
                &context,
            )
            .await?;
        let image = held
            .call(
                "image",
                json!({"entry":entry,"message":message,"part":part}).into(),
                &context,
            )
            .await?;
        Ok::<Image, Error>(serde_json::from_value(image.to_serde()?)?)
    }
    .await;
    let image = match result {
        Ok(image) => image,
        Err(error) => {
            let mut response = json_response(&json!({"error":error.to_string()}))?;
            *response.status_mut() = http::StatusCode::NOT_FOUND;
            return Ok(response);
        }
    };
    let body = STANDARD
        .decode(image.data)
        .map_err(|_| HttpError::new(404, "No such image"))?;
    let mime = match image.mime_type.as_str() {
        safe @ ("image/png" | "image/jpeg" | "image/gif" | "image/webp") => safe,
        _ => "application/octet-stream",
    };
    let mut response = HttpResponse::new(Default::default());
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(mime)?);
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&body.len().to_string())?,
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if request.method != Method::HEAD {
        *response.body_mut() = body.into();
    }
    Ok(response)
}

pub(super) async fn image_at(
    conversation: &Conversation,
    place: Place,
    ctx: &Context,
) -> Result<Value> {
    let entries = conversation
        .entries(
            EntryQuery {
                min_entry_id: Some(place.entry),
                max_entry_id: Some(place.entry),
                ..Default::default()
            },
            1,
            None,
            ctx,
        )
        .await
        .map_err(|error| Error::Invalid(error.to_string()))?;
    let image = entries
        .items
        .first()
        .and_then(|entry| entry.model.as_ref()?.get(place.message))
        .filter(|message| message["role"] != "assistant")
        .and_then(|message| message["content"].as_array()?.get(place.part))
        .filter(|part| part["type"] == "image")
        .ok_or_else(|| Error::Invalid("No such image".into()))?;
    Ok(json!({"mimeType":image["mimeType"],"data":image["data"]}))
}
