use http::{HeaderValue, StatusCode, header};
use pi_rs_chord::node::http::{HttpResponse, HttpResult, html_response};
use url::Url;

pub(super) fn result(title: &str, message: &str) -> String {
    document(
        title,
        &format!("<h1>{}</h1><p>{}</p>", escape(title), escape(message)),
    )
}

pub(super) fn send(status: StatusCode, html: String, extra: &str) -> HttpResult<HttpResponse> {
    let mut response = html_response(html)?;
    *response.status_mut() = status;
    response.headers_mut().insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_str(&format!(
        "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'{extra}"
    ))?);
    Ok(response)
}

pub(super) fn probe(name: &str, url: &Url, nonce: &str) -> HttpResult<HttpResponse> {
    let title = format!("Sign in to {name}");
    let probe = serde_json::to_string(url.as_str())?.replace('<', "\\u003c");
    let body = format!(
        r##"<h1>{}</h1>
<p id="checking">Checking whether this browser runs on the p2 computer…</p>
<div id="choose" hidden><p>Where does this browser run?</p>
<p><a href="?via=loopback">On the p2 computer</a></p>
<p><a href="?via=callback">On another device</a></p></div>
<noscript><p><a href="?via=loopback">This browser runs on the p2 computer</a></p>
<p><a href="?via=callback">This browser runs on another device</a></p></noscript>
<script nonce="{nonce}">
let chosen = false;
const go = (via) => {{ if (!chosen) {{ chosen = true; location.replace("?via=" + via); }} }};
for (const link of document.querySelectorAll("#choose a")) link.addEventListener("click", () => {{ chosen = true; }});
const slow = setTimeout(() => {{ document.getElementById("checking").hidden = true; document.getElementById("choose").hidden = false; }}, 1500);
fetch({probe}, {{ cache: "no-store" }}).then(
    (answer) => {{ clearTimeout(slow); go(answer.ok ? "loopback" : "callback"); }},
    () => {{ clearTimeout(slow); go("callback"); }},
);
</script>"##,
        escape(&title)
    );
    send(
        StatusCode::OK,
        document(&title, &body),
        &format!(
            "; script-src 'nonce-{nonce}'; connect-src {}",
            url.origin().ascii_serialization()
        ),
    )
}

fn document(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>{}</title>
<style>body{{font:16px/1.5 system-ui,sans-serif;max-width:32rem;margin:4rem auto;padding:0 1rem;color:#222}}a{{color:#0a58ca}}@media(prefers-color-scheme:dark){{body{{background:#111;color:#ddd}}a{{color:#6ea8fe}}}}</style>
</head><body>{body}</body></html>"#,
        escape(title)
    )
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
