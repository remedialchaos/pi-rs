use super::RemoteInfo;

fn root(path: &str, windows: bool) -> (&str, &str) {
    if windows {
        if let Some(unc) = path.strip_prefix("//") {
            let mut parts = unc.splitn(3, '/');
            if let (Some(server), Some(share)) = (parts.next(), parts.next())
                && !server.is_empty()
                && !share.is_empty()
            {
                let end = 2 + server.len() + 1 + share.len();
                return (&path[..end], path[end..].trim_start_matches('/'));
            }
        }
        if path.as_bytes().get(1) == Some(&b':') && path.as_bytes()[0].is_ascii_alphabetic() {
            return path.split_at(2);
        }
    }
    if let Some(rest) = path.strip_prefix('/') {
        ("/", rest)
    } else {
        ("", path)
    }
}
fn normalize(path: &str, win: bool, trailing: bool) -> String {
    let (prefix, rest) = root(path, win);
    let absolute = prefix == "/" || prefix.starts_with("//") || rest.starts_with('/');
    let mut parts = Vec::new();
    for p in rest.split('/') {
        match p {
            "" | "." => {}
            ".." if parts.last().is_some_and(|x| *x != "..") => {
                parts.pop();
            }
            ".." if absolute => {}
            p => parts.push(p),
        }
    }
    let mut out = prefix.to_string();
    if absolute && !out.ends_with('/') {
        out.push('/');
    }
    out.push_str(&parts.join("/"));
    if out.is_empty() || (win && !absolute && parts.is_empty()) {
        out.push('.');
    }
    if trailing && path.ends_with('/') && !out.ends_with('/') {
        out.push('/');
    }
    if win { out.replace('/', "\\") } else { out }
}
fn slashes(path: &str, win: bool) -> String {
    if win {
        path.replace('\\', "/")
    } else {
        path.into()
    }
}
pub(super) fn join(info: &RemoteInfo, parts: &[&str]) -> String {
    let win = info.separator == "\\";
    let mut path = slashes(
        &parts
            .iter()
            .copied()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("/"),
        win,
    );
    if win
        && !parts.iter().find(|p| !p.is_empty()).is_some_and(|p| {
            let s = slashes(p, true);
            s.starts_with("//") && s.as_bytes().get(2).is_some_and(|b| *b != b'/')
        })
        && path.starts_with("//")
    {
        path = format!("/{}", path.trim_start_matches('/'));
    }
    normalize(&path, win, true)
}
fn file_url(path: &str, win: bool) -> Option<String> {
    let url = url::Url::parse(path).ok()?;
    let encoded = url.path();
    if encoded.to_ascii_lowercase().contains("%2f")
        || win && encoded.to_ascii_lowercase().contains("%5c")
    {
        return None;
    }
    for (i, b) in encoded.bytes().enumerate() {
        if b == b'%'
            && !encoded
                .as_bytes()
                .get(i + 1..i + 3)
                .is_some_and(|p| p.iter().all(u8::is_ascii_hexdigit))
        {
            return None;
        }
    }
    let decoded = percent_encoding::percent_decode_str(encoded)
        .decode_utf8()
        .ok()?;
    if let Some(host) = url.host_str() {
        return win.then(|| format!("//{host}{decoded}"));
    }
    if win {
        let p = decoded.strip_prefix('/')?;
        return (p.as_bytes().get(1) == Some(&b':')).then(|| p.into());
    }
    Some(decoded.into_owned())
}
pub(super) fn resolve(info: &RemoteInfo, cwd: &str, path: &str) -> String {
    let win = info.separator == "\\";
    let mut p = slashes(path, win);
    if p == "~" {
        p = slashes(&info.home, win);
    } else if let Some(rest) = p.strip_prefix("~/") {
        p = format!(
            "{}/{}",
            slashes(&info.home, win),
            rest.trim_start_matches('/')
        );
    } else if path.starts_with("file://")
        && let Some(decoded) = file_url(path, win)
    {
        p = slashes(&decoded, win);
    }
    let cwd = slashes(cwd, win);
    let base = if root(&cwd, win).0.is_empty() {
        format!("{}/{cwd}", slashes(&info.cwd, win))
    } else {
        cwd
    };
    let (drive, rest) = root(&p, win);
    let absolute = if drive == "/" && win {
        format!("{}{}", root(&slashes(&info.cwd, win), win).0, p)
    } else if drive == "/" || drive.starts_with("//") || rest.starts_with('/') {
        p
    } else if !drive.is_empty() {
        let mut base = if root(&base, win).0.eq_ignore_ascii_case(drive) {
            base
        } else {
            slashes(
                info.drive_cwds
                    .get(&drive.to_uppercase())
                    .and_then(|s| s.as_str())
                    .unwrap_or(&info.cwd),
                win,
            )
        };
        if !root(&base, win).0.eq_ignore_ascii_case(drive) {
            base = drive.into();
        }
        format!("{base}/{rest}")
    } else {
        format!("{base}/{p}")
    };
    normalize(&absolute, win, false)
}
