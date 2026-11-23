//! The HTTP answers p3's routes give, and the principal a request authenticated as.

use http::{HeaderValue, StatusCode};
use pi_rs_chord::node::http::{
    HttpError, HttpRequest, HttpResponse, HttpResult, html_response, json_response, text_response,
};
use serde_json::Value;

use crate::principals::Principal;

/// A JSON answer with `status`.
pub fn json(status: u16, value: &Value) -> HttpResult<HttpResponse> {
    let mut response = json_response(value)?;
    set_status(&mut response, status)?;
    Ok(response)
}

/// An HTML answer: the request's nonce signs its script.
pub fn html(body: String) -> HttpResult<HttpResponse> {
    html_response(body)
}

/// An error answer with `status` and a short text.
pub fn error(status: u16, message: &str) -> HttpResult<HttpResponse> {
    let mut response = text_response(status, message.to_string())?;
    set_status(&mut response, status)?;
    Ok(response)
}

/// A response with `status` instead of 200.
pub fn set_status(response: &mut HttpResponse, status: u16) -> HttpResult<()> {
    *response.status_mut() = StatusCode::from_u16(status)
        .map_err(|_| HttpError::new(500, "invalid status in an answer"))?;
    Ok(())
}

/// Adds a header to a response.
pub fn set_header(response: &mut HttpResponse, name: &str, value: &str) -> HttpResult<()> {
    let name = http::HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| HttpError::new(500, "invalid header name"))?;
    let value =
        HeaderValue::from_str(value).map_err(|_| HttpError::new(500, "invalid header value"))?;
    response.headers_mut().insert(name, value);
    Ok(())
}

/// The principal a request authenticated as.
pub fn principal(request: &HttpRequest) -> Option<Principal> {
    request.principal.as_ref().and_then(Principal::from_value)
}

/// The request's cookie header.
pub fn cookie_header(request: &HttpRequest) -> Option<&str> {
    request.header("cookie")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_answer_carries_the_status_and_content_type_it_was_given() {
        let response = json(403, &json!({"error": "no"})).expect("json");
        assert_eq!(response.status(), 403);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/json; charset=utf-8")
        );
        let response = error(503, "later").expect("error");
        assert_eq!(response.status(), 503);
        let response = html("<p>hi</p>".into()).expect("html");
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/html; charset=utf-8")
        );
    }
}
