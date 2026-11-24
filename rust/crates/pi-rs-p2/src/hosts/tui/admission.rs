use reqwest::{Client, Url, header};
use serde_json::{Value, json};
use std::{
    error::Error,
    fs,
    io::{self, Write},
    path::Path,
};

pub(super) fn load(path: &Path) -> Result<Value, Box<dyn Error>> {
    let input: Value = match fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|_| "Cannot read p2-tui config.json")?
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(json!({"connections":{}})),
        Err(e) => return Err(e.into()),
    };
    let invalid = "Cannot read p2-tui config.json";
    let mut cleaned = json!({"connections":{}});
    for (origin, entry) in input["connections"].as_object().ok_or(invalid)? {
        let obj = entry.as_object().ok_or(invalid)?;
        let mut connection = json!({});
        if let Some(value) = obj.get("cookie") {
            let cookie = value.as_str().ok_or(invalid)?;
            if !cookie.is_empty() {
                connection["cookie"] = json!(cookie);
            }
        }
        if let Some(cookie) = entry["guest"]["cookie"].as_str() {
            if cookie.is_empty()
                || cookie.len() > 4096
                || !cookie.bytes().all(|b| b.is_ascii_graphic())
            {
                return Err(invalid.into());
            }
            connection["guest"] = json!({"cookie":cookie});
            if let Some(session) = entry["guest"]["session"]
                .as_str()
                .filter(|s| valid_session(s))
            {
                connection["guest"]["session"] = json!(session);
            }
        }
        cleaned["connections"][origin] = connection;
    }
    Ok(cleaned)
}
fn valid_session(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}
pub(super) fn save(
    path: &Path,
    origin: &str,
    field: &str,
    value: Option<&Value>,
    previous: &Value,
) -> Result<(), Box<dyn Error>> {
    let dir = path.parent().ok_or("Invalid config directory")?;
    let mut dirs = fs::DirBuilder::new();
    dirs.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        dirs.mode(0o700);
    }
    dirs.create(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("config.lock"))?;
    lock.lock()?;
    let mut config = load(path)?;
    if let Some(value) = value {
        config["connections"][origin][field] = value.clone();
    } else if &config["connections"][origin][field] == previous
        && let Some(entry) = config["connections"][origin].as_object_mut()
    {
        entry.remove(field);
    }
    let tmp = dir.join(uuid::Uuid::new_v4().to_string());
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    let result = (|| -> Result<(), Box<dyn Error>> {
        serde_json::to_writer_pretty(&mut f, &config)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

pub(super) async fn join(
    client: &Client,
    url: &Url,
    link: &Url,
    stored: &Value,
) -> Result<Value, Box<dyn Error>> {
    let token = link
        .path()
        .strip_prefix("/join/")
        .ok_or("Invalid share link")?;
    if url.origin() != link.origin()
        || !link.username().is_empty()
        || link.password().is_some()
        || link.query().is_some()
        || link.fragment().is_some()
        || token.len() != 43
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err("Use a share link for this origin".into());
    }
    let name = loop {
        eprint!("Name: ");
        io::stderr().flush()?;
        let mut s = String::new();
        if io::stdin().read_line(&mut s)? == 0 {
            return Err("Missing guest name".into());
        }
        let s: String = s.chars().filter(|c| !c.is_control()).collect();
        let name = String::from_utf16_lossy(&s.trim().encode_utf16().take(40).collect::<Vec<_>>());
        if !name.is_empty() {
            break name;
        }
    };
    let mut request = client
        .post(url.join("/join")?)
        .header(header::ORIGIN, url.origin().ascii_serialization());
    if let Some(cookie) = stored["cookie"].as_str() {
        request = request.header(header::COOKIE, cookie);
    }
    let mut response = request
        .json(&json!({"token":token,"name":name}))
        .send()
        .await?
        .error_for_status()?;
    if !response.status().is_success() {
        return Err("Join failed".into());
    }
    let cookie = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok()?.split(';').next())
        .find(|s| {
            s.split_once('=')
                .and_then(|(k, _)| k.strip_prefix("p2_guest_"))
                .is_some_and(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
        })
        .or_else(|| stored["cookie"].as_str())
        .ok_or("Missing guest cookie")?
        .to_owned();
    if cookie.is_empty() || cookie.len() > 4096 || !cookie.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("Invalid guest cookie".into());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > 2048 - body.len() {
            return Err("Join response exceeds its size limit".into());
        }
        body.extend_from_slice(&chunk);
    }
    let body: Value = serde_json::from_slice(&body)?;
    let session = body["session"]
        .as_str()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        })
        .ok_or("Invalid guest session")?;
    Ok(json!({"cookie":cookie,"session":session}))
}

pub(super) async fn claim(
    client: &Client,
    url: &Url,
    link: &Url,
    stored: &Value,
) -> Result<String, Box<dyn Error>> {
    let origin = url.origin().ascii_serialization();

    let token = link.fragment().ok_or("Missing owner token")?;
    if link.origin() != url.origin()
        || !link.username().is_empty()
        || link.password().is_some()
        || link.path() != "/owner"
        || link.query().is_some()
        || token.len() != 64
        || !token
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err("Use an owner link for this origin with its token in the fragment".into());
    }
    let request = client
        .post(url.join("/owner/claim")?)
        .header(header::ORIGIN, &origin)
        .json(&json!({"token": token}));
    let request = if let Some(cookie) = stored["cookie"].as_str() {
        request.header(header::COOKIE, cookie)
    } else {
        request
    };
    let response = request.send().await?;
    if !response.status().is_success() {
        return Err("Owner claim failed".into());
    }
    super::response_cookie(&response)
}

pub(super) async fn redeem(
    client: &Client,
    url: &Url,
    link: &Url,
    config: &mut Value,
    path: &Path,
    guest: bool,
) -> Result<(String, Option<String>), Box<dyn Error>> {
    let origin = url.origin().ascii_serialization();
    let result = if guest {
        let guest = join(client, url, link, &config["connections"][&origin]["guest"]).await?;
        let cookie = guest["cookie"]
            .as_str()
            .ok_or("Missing guest cookie")?
            .to_owned();
        let session = guest["session"].as_str().map(str::to_owned);
        config["connections"][&origin]["guest"] = guest;
        (cookie, session)
    } else {
        let cookie = claim(client, url, link, &config["connections"][&origin]).await?;
        config["connections"][&origin]["cookie"] = json!(cookie);
        (cookie, None)
    };
    let field = if guest { "guest" } else { "cookie" };
    save(
        path,
        &origin,
        field,
        Some(&config["connections"][&origin][field]),
        &Value::Null,
    )?;
    Ok(result)
}
