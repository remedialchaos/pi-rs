//! Project place matching helpers matching `apps/p2/plugins/projects/contract.ts` & `place.ts`.

use super::config::{Project, ProjectPlace};

pub const LOCAL: &str = "local";

pub fn place_label<'a>(place: &'a ProjectPlace, local_label: &'a str) -> &'a str {
    place.env.as_deref().unwrap_or(local_label)
}

fn is_within(path: &str, root: &str) -> bool {
    let base = root.trim_end_matches(['/', '\\']);
    path == root
        || path == base
        || path
            .strip_prefix(base)
            .is_some_and(|rest| rest.starts_with('/') || rest.starts_with('\\'))
}

/// Returns the subpath of `path` below `root` (`""` at root), or `None` outside it.
pub fn below(path: &str, root: &str) -> Option<String> {
    let rest = |from: usize| path[from..].trim_start_matches(['/', '\\']).to_owned();
    if let Some(after_tilde) = root.strip_prefix('~') {
        let normalized = after_tilde.replace('\\', "/");
        let relative = normalized.trim_matches('/');
        if relative.is_empty() {
            return Some(String::new());
        }
        let text = path.replace('\\', "/");
        let needle = format!("/{relative}");
        let mut search = 1usize;
        while search < text.len() {
            let Some(offset) = text[search..].find(&needle) else {
                break;
            };
            let index = search + offset;
            let end = index + needle.len();
            if end == text.len() || text.as_bytes()[end] == b'/' {
                return Some(rest(end));
            }
            search = index + 1;
        }
        return None;
    }
    if is_within(path, root) {
        let base_len = root.trim_end_matches(['/', '\\']).len();
        Some(rest(base_len))
    } else {
        None
    }
}

pub fn place_on<'a>(
    project: &'a Project,
    name: &str,
    local_label: &str,
) -> Option<&'a ProjectPlace> {
    if let Some(remote) = project
        .places
        .iter()
        .find(|place| place.env.as_deref() == Some(name))
    {
        return Some(remote);
    }
    let here = name == LOCAL || name.eq_ignore_ascii_case(local_label);
    if here {
        project.places.iter().find(|place| place.env.is_none())
    } else {
        None
    }
}

fn depth_of(place: &ProjectPlace) -> usize {
    place
        .path
        .split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != "~")
        .count()
}

pub fn project_at<'a>(
    projects: &'a [Project],
    env: Option<&str>,
    cwd: &str,
    member: Option<&str>,
) -> Option<&'a Project> {
    if let Some(name) = member
        && let Some(joined) = projects.iter().find(|project| project.name == name)
    {
        return Some(joined);
    }
    let mut closest: Option<(&'a Project, usize)> = None;
    for project in projects {
        for place in &project.places {
            if place.env.as_deref() != env || below(cwd, &place.path).is_none() {
                continue;
            }
            let depth = depth_of(place);
            if closest.is_none_or(|(_, best)| depth > best) {
                closest = Some((project, depth));
            }
        }
    }
    closest.map(|(project, _)| project)
}
