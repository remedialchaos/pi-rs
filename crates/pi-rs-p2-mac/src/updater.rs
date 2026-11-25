use crate::{Error, Result, http};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use roxmltree::Document;
use std::{cmp::Ordering, time::Duration};
use url::Url;

const SPARKLE: &str = "http://www.andymatuschak.org/xml-namespaces/sparkle";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub version: String,
    pub short_version: Option<String>,
    pub title: Option<String>,
    pub url: Url,
    pub length: u64,
    pub signature: String,
    pub minimum_system: Option<String>,
    pub maximum_system: Option<String>,
    pub hardware: Option<String>,
}

/// Pi builds use a single decimal UTC minute, ordered as an integer; dotted bundle versions are component-wise.
pub fn compare_versions(left: &str, right: &str) -> Result<Ordering> {
    let parts = |s: &str| {
        s.split('.')
            .map(|p| {
                if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) {
                    p.parse::<u64>().ok()
                } else {
                    None
                }
            })
            .collect::<Option<Vec<_>>>()
    };
    let invalid = || Error::Protocol("invalid CFBundleVersion".into());
    let mut left = parts(left).ok_or_else(invalid)?;
    let mut right = parts(right).ok_or_else(invalid)?;
    while left.last() == Some(&0) {
        left.pop();
    }
    while right.last() == Some(&0) {
        right.pop();
    }
    Ok(left.cmp(&right))
}

pub fn parse_appcast(xml: &str, feed: &Url) -> Result<Vec<Update>> {
    let doc = Document::parse(xml)
        .map_err(|e| Error::Protocol(format!("invalid Sparkle appcast: {e}")))?;
    let root = doc.root_element();
    if !root.has_tag_name("rss") {
        return Err(Error::Protocol("appcast is not RSS".into()));
    }
    let channel = root
        .children()
        .find(|n| n.has_tag_name("channel"))
        .ok_or_else(|| Error::Protocol("appcast has no channel".into()))?;
    let mut updates = Vec::new();
    for item in channel.children().filter(|n| n.has_tag_name("item")) {
        let field = |name| {
            item.children()
                .find(|n| n.has_tag_name((SPARKLE, name)))
                .and_then(|n| n.text())
                .map(str::trim)
        };
        let title = item
            .children()
            .find(|n| n.has_tag_name("title"))
            .and_then(|n| n.text())
            .map(str::to_owned);
        for enclosure in item.children().filter(|n| n.has_tag_name("enclosure")) {
            if enclosure.attribute((SPARKLE, "deltaFrom")).is_some() {
                continue;
            }
            let Some(version) =
                field("version").or_else(|| enclosure.attribute((SPARKLE, "version")))
            else {
                continue;
            };
            let Some((url, signature, length)) = enclosure
                .attribute("url")
                .zip(enclosure.attribute((SPARKLE, "edSignature")))
                .zip(
                    enclosure
                        .attribute("length")
                        .and_then(|s| s.parse::<u64>().ok()),
                )
                .map(|((url, signature), length)| (url, signature, length))
            else {
                continue;
            };
            compare_versions(version, version)?;
            let url = feed.join(url)?;
            if !crate::radius_account::allowed_endpoint(&url) {
                return Err(Error::Protocol("unsafe update URL".into()));
            }
            updates.push(Update {
                version: version.into(),
                short_version: field("shortVersionString")
                    .or_else(|| enclosure.attribute((SPARKLE, "shortVersionString")))
                    .map(str::to_owned),
                title: title.clone(),
                url,
                length,
                signature: signature.into(),
                minimum_system: field("minimumSystemVersion").map(str::to_owned),
                maximum_system: field("maximumSystemVersion").map(str::to_owned),
                hardware: field("hardwareRequirements").map(str::to_owned),
            });
        }
    }
    Ok(updates)
}

pub fn select(
    updates: &[Update],
    current: &str,
    system: Option<&str>,
    architecture: &str,
) -> Result<Option<Update>> {
    let mut selected: Option<&Update> = None;
    for update in updates {
        if compare_versions(&update.version, current)? != Ordering::Greater {
            continue;
        }
        if let Some(requirement) = &update.hardware
            && !requirement.split_whitespace().any(|r| r == architecture)
        {
            continue;
        }
        if let Some(system) = system {
            if let Some(min) = &update.minimum_system
                && compare_versions(system, min)? == Ordering::Less
            {
                continue;
            }
            if let Some(max) = &update.maximum_system
                && compare_versions(system, max)? == Ordering::Greater
            {
                continue;
            }
        }
        if let Some(previous) = selected
            && compare_versions(&update.version, &previous.version)? != Ordering::Greater
        {
            continue;
        }
        selected = Some(update);
    }
    Ok(selected.cloned())
}

fn verify(update: &Update, key: &str, bytes: &[u8]) -> Result<()> {
    let invalid = |s: &str| Error::Protocol(s.into());
    if bytes.len() as u64 != update.length {
        return Err(invalid("update length does not match the appcast"));
    }
    let key: [u8; 32] = STANDARD
        .decode(key.trim())
        .ok()
        .and_then(|key| key.try_into().ok())
        .ok_or_else(|| invalid("invalid SUPublicEDKey"))?;
    let key = VerifyingKey::from_bytes(&key).map_err(|_| invalid("invalid SUPublicEDKey"))?;
    let signature = STANDARD
        .decode(update.signature.trim())
        .ok()
        .and_then(|sig| Signature::from_slice(&sig).ok())
        .ok_or_else(|| invalid("invalid Sparkle Ed25519 signature"))?;
    key.verify_strict(bytes, &signature)
        .map_err(|_| invalid("update Ed25519 signature could not be verified"))
}

pub fn verify_archive(update: &Update, key: &str, bytes: &[u8]) -> Result<()> {
    verify(update, key, bytes)
}

pub async fn check(
    feed: &Url,
    current: &str,
    system: Option<&str>,
    architecture: &str,
) -> Result<Option<Update>> {
    if !crate::radius_account::allowed_endpoint(feed) {
        return Err(Error::Protocol("unsafe appcast URL".into()));
    }
    let response = http(Duration::from_secs(30))?
        .get(feed.clone())
        .send()
        .await?
        .error_for_status()?;
    let xml = response.text().await?;
    select(&parse_appcast(&xml, feed)?, current, system, architecture)
}

pub async fn download_verified(update: &Update, key: &str) -> Result<Vec<u8>> {
    let mut response = http(Duration::from_secs(180))?
        .get(update.url.clone())
        .send()
        .await?
        .error_for_status()?;
    if response
        .content_length()
        .is_some_and(|n| n != update.length)
    {
        return Err(Error::Protocol(
            "update length does not match the appcast".into(),
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() as u64 + chunk.len() as u64 > update.length {
            return Err(Error::Protocol(
                "update length does not match the appcast".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    verify(update, key, &bytes)?;
    Ok(bytes)
}
