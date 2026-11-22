use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey};
use pi_rs_p2_mac::{
    appearance::{Appearance, fade},
    brain::Status,
    brain_directory::BrainDirectory,
    data_backup,
    diagnostics::{Diagnostics, bridge_sample},
    host_bridge::{self, Message},
    server_config::{Bundled, Environment, JsonPreferences, Launch, Mode, Preferences, Source},
    updater,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, time::Duration};
use url::Url;
mod support;
use support::{Loopback, Test, sandbox};

#[tokio::test]
async fn updater_downloads_and_verifies_the_actual_enclosure_from_loopback() -> Test {
    use axum::{Router, routing::get};
    let archive = b"signed archive";
    let key = SigningKey::from_bytes(&[17; 32]);
    let signature = STANDARD.encode(key.sign(archive).to_bytes());
    let feed = format!(
        r#"<rss xmlns:s="http://www.andymatuschak.org/xml-namespaces/sparkle"><channel><item>
        <s:version>202611230610</s:version><enclosure url="/archive" length="{}" s:edSignature="{signature}"/>
      </item></channel></rss>"#,
        archive.len()
    );
    let router = Router::new()
        .route(
            "/appcast",
            get(move || {
                let feed = feed.clone();
                async move { feed }
            }),
        )
        .route("/archive", get(|| async { &b"signed archive"[..] }));
    let server = Loopback::serve(router).await?;
    let update = updater::check(&server.url.join("appcast")?, "202611230609", None, "arm64")
        .await?
        .ok_or("no update")?;
    let bytes =
        updater::download_verified(&update, &STANDARD.encode(key.verifying_key().as_bytes()))
            .await?;
    assert_eq!(bytes, archive);
    Ok(())
}

#[test]
fn attach_uses_its_own_directory_but_never_changes_own_server_precedence() -> Test {
    let tmp = sandbox()?;
    let checkout = tmp.path().join("repo/apps/p2");
    fs::create_dir_all(checkout.join("src/hosts"))?;
    fs::write(checkout.join("src/hosts/server.ts"), "")?;
    let mut preferences = JsonPreferences::open(tmp.path().join("preferences.json"))?;
    preferences.set("source", Some("attached"))?;
    preferences.set("attachedPort", Some("8765"))?;
    preferences.set("attachedDataDir", Some("~/saved-attach"))?;
    preferences.set("dataDir", Some("~/saved-own"))?;
    preferences.set("mode", Some("dev"))?;
    let mut env = Environment::from([
        ("HOME".into(), tmp.path().to_string_lossy().into()),
        ("P2MAC_ATTACH".into(), "9876".into()),
        ("P2MAC_PORT".into(), "5678".into()),
        ("P2MAC_DATA_DIR".into(), "~/env-data".into()),
        ("P2MAC_CHECKOUT".into(), "~/repo".into()),
        ("P2MAC_FAKE".into(), "1".into()),
    ]);
    let loaded = Launch::load(&env, &preferences, None)?;
    assert_eq!(loaded.selected.source, Source::Attached);
    assert_eq!(loaded.selected.port, 9876);
    assert_eq!(loaded.selected.data_dir, tmp.path().join("env-data"));
    assert_eq!(loaded.own.port, 5678);
    assert_eq!(loaded.own.data_dir, tmp.path().join("saved-own"));
    assert_eq!(loaded.own.mode, Mode::Dev);
    assert_eq!(loaded.own.checkout, Some(checkout));
    assert!(loaded.own.fake);
    env.remove("P2MAC_ATTACH");
    let loaded = Launch::load(&env, &preferences, None)?;
    assert_eq!(loaded.selected.port, 8765);
    assert_eq!(loaded.selected.data_dir, tmp.path().join("saved-attach"));
    assert_eq!(loaded.own.data_dir, tmp.path().join("env-data"));
    loaded.own.save_selection(&mut preferences)?;
    let reopened = JsonPreferences::open(tmp.path().join("preferences.json"))?;
    assert_eq!(
        Launch::load(&env, &reopened, None)?.selected.source,
        Source::Own
    );
    Ok(())
}

#[test]
fn bundled_fallback_uses_its_node_and_preserves_default_port_mode_and_path() -> Test {
    let home = sandbox()?;
    let env = Environment::from([
        ("HOME".into(), home.path().to_string_lossy().into()),
        ("P2MAC_CHECKOUT".into(), "~/does-not-exist".into()),
    ]);
    let bundled = Bundled {
        checkout: home.path().join("bundled"),
        node: home.path().join("bundled-node"),
    };
    let launch = Launch::load(&env, &BTreeMap::new(), Some(&bundled))?;
    assert_eq!(launch.selected.port, 8788);
    assert_eq!(launch.selected.mode, Mode::Start);
    assert_eq!(launch.selected.data_dir, home.path().join(".pi/p2"));
    assert_eq!(launch.selected.node, Some(bundled.node));
    assert!(launch.selected.runs_bundled);
    Ok(())
}

#[test]
fn backup_clones_previous_build_and_retains_three_real_copies() -> Test {
    let root = sandbox()?;
    let data = root.path().join("p2");
    fs::create_dir_all(data.join("sessions"))?;
    #[cfg(unix)]
    let _socket = std::os::unix::net::UnixListener::bind(data.join("dev.sock"))?;
    let mut preferences = BTreeMap::new();
    assert_eq!(
        data_backup::before_upgrade(&data, "202611170001", &mut preferences)?,
        None
    );
    for i in 1..=5 {
        let previous = format!("2026111700{i:02}");
        let next = format!("2026111700{:02}", i + 1);
        fs::write(
            data.join("sessions/example"),
            format!("data for {previous}"),
        )?;
        let _ = data_backup::before_upgrade(&data, &next, &mut preferences)?;
        let target = root.path().join("p2.backups").join(&previous);
        assert_eq!(
            fs::read_to_string(target.join("sessions/example"))?,
            format!("data for {previous}")
        );
        assert!(!target.join("dev.sock").exists());
    }
    let backup = root.path().join("p2.backups");
    let mut builds = fs::read_dir(backup)?
        .map(|d| d.map(|d| d.file_name()))
        .collect::<Result<Vec<_>, _>>()?;
    builds.sort();
    assert_eq!(builds, ["202611170003", "202611170004", "202611170005"]);
    assert_eq!(
        data_backup::before_upgrade(&data, "202611170006", &mut preferences)?,
        None
    );
    assert!(data_backup::backup(&data, "../outside").is_err());
    assert!(data.join("sessions/example").exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn backup_does_not_traverse_symlinks_in_backup_root() -> Test {
    let root = sandbox()?;
    let data = root.path().join("p2");
    let other = root.path().join("outside");
    fs::create_dir(&data)?;
    fs::create_dir(&other)?;
    fs::write(other.join("preserve"), "original")?;
    std::os::unix::fs::symlink(&other, root.path().join("p2.backups"))?;
    assert!(data_backup::backup(&data, "42").is_err());
    assert_eq!(fs::read_to_string(other.join("preserve"))?, "original");
    assert!(!other.join("42").exists());
    Ok(())
}

#[test]
fn bridge_allows_only_exact_origin_main_frame_and_known_appearance() -> Test {
    let expected = Url::parse("http://127.0.0.1:8788/path")?;
    let actual = Url::parse("http://127.0.0.1:8788/another")?;
    let message = json!({"type":"set-theme","preference":"dark"});
    assert_eq!(
        host_bridge::admit(Some(&expected), &actual, true, &message)?,
        Message::SetTheme(Appearance::Dark)
    );
    for (origin, main) in [
        (actual.clone(), false),
        (Url::parse("http://127.0.0.1:8789/")?, true),
        (Url::parse("https://127.0.0.1:8788/")?, true),
        (Url::parse("http://localhost:8788/")?, true),
        (Url::parse("about:blank")?, true),
        (Url::parse("blob:http://127.0.0.1:8788/object")?, true),
    ] {
        let Err(error) = host_bridge::admit(Some(&expected), &origin, main, &message) else {
            return Err("bridge accepted an untrusted origin".into());
        };
        assert_eq!(error.to_string(), "piHost: not allowed here");
    }
    for invalid in [
        json!({"type":"set-theme","preference":"sepia"}),
        json!({"type":"set-theme","preference":true}),
        json!({"type":"set-theme"}),
    ] {
        let Err(error) = host_bridge::admit(Some(&expected), &actual, true, &invalid) else {
            return Err("bridge accepted invalid preference".into());
        };
        assert_eq!(error.to_string(), "piHost: invalid preference");
    }
    assert_eq!(fade(true, true, false), Some(Duration::from_millis(250)));
    assert!(
        [
            fade(false, true, false),
            fade(true, false, false),
            fade(true, true, true)
        ]
        .into_iter()
        .all(|f| f.is_none())
    );
    Ok(())
}

#[test]
fn fixture_brains_match_local_identity_cluster_labels_and_ignore_cloud() -> Test {
    let root = sandbox()?;
    let data = root.path().join("data");
    fs::create_dir(&data)?;
    fs::write(data.join("radius.json"), r#"{"serverId":"beta"}"#)?;
    fs::write(data.join("identity.json"), r#"{"name":"fallback"}"#)?;
    let path = root.path().join("brains.json");
    fs::write(
        &path,
        serde_json::to_vec(&json!({"servers":[
        {"serverId":"beta","name":"Mine","organization":{"id":"z","name":"Zeta"},"status":"offline","shared":true},
        {"serverId":"aardvark","name":"Someone Else","organization":{"id":"z","name":"Zeta"},"status":"offline","shared":true},
        {"serverId":"0123456789","organization":{"id":"a","name":"Alpha"},"status":"connecting"}
    ],"cloudBrains":[{"serverId":"excluded"}]}))?,
    )?;
    let env = Environment::from([("P2MAC_DATA_DIR".into(), data.to_string_lossy().into())]);
    let config = Launch::load(&env, &BTreeMap::new(), None)?.selected;
    let mut directory = BrainDirectory::new(Some(&path));
    directory.rebuild(&config, true);
    assert!(directory.shows_tabs);
    assert_eq!(
        directory.tab_labels().collect::<Vec<_>>(),
        [
            "Alpha / 01234567",
            "Zeta / Shared · Someone Else",
            "Zeta / Mine"
        ]
    );
    let Some(local) = directory.local() else {
        return Err("fixture lost local Pi".into());
    };
    assert_eq!(local.id, "beta");
    assert_eq!(local.status, Status::Online);
    assert!(!local.shared);
    assert!(directory.brains[0].away());
    assert!(directory.brains[1].away());
    fs::remove_file(data.join("radius.json"))?;
    directory.rebuild(&config, false);
    assert_eq!(directory.brains[0].name, "fallback");
    assert_eq!(directory.brains[0].id, "local-8788");
    assert_eq!(directory.brains[0].status, Status::Offline);
    Ok(())
}

#[test]
fn diagnostics_records_portable_bridge_replies_and_refusals_under_numbered_paths() -> Test {
    let root = sandbox()?;
    let env = Environment::from([
        (
            "P2MAC_DIAG_DIR".into(),
            root.path().to_string_lossy().into(),
        ),
        ("P2MAC_DIAG_BRIDGE".into(), "dark".into()),
    ]);
    let launch = Launch::load(&env, &BTreeMap::new(), None)?;
    let Some(mut diagnostics) = Diagnostics::new(&launch) else {
        return Err("diagnostics not configured".into());
    };
    assert_eq!(diagnostics.consume_bridge(false), None);
    let Some(preference) = diagnostics.consume_bridge(true) else {
        return Err("missing requested appearance".into());
    };
    let (index, _) = diagnostics.next_page();
    let record = bridge_sample(&launch.selected.origin(), Appearance::System, &preference)?;
    diagnostics.bridge(index, &record)?;
    let parsed: Value = serde_json::from_slice(&fs::read(root.path().join("bridge-1.json"))?)?;
    assert_eq!(
        parsed["page"]["before"]["get"]["reply"]["preference"],
        "system"
    );
    assert_eq!(parsed["page"]["events"], json!([{"preference":"dark"}]));
    assert_eq!(
        parsed["page"]["fromFrame"]["error"],
        "piHost: not allowed here"
    );
    assert_eq!(parsed["foreign"]["error"], "piHost: not allowed here");
    assert_eq!(diagnostics.consume_bridge(true), None);
    Ok(())
}

#[test]
fn sparkle_feed_selects_newer_compatible_minute_and_verifies_signed_archive() -> Test {
    let archive = b"offline update archive fixture";
    let signing = SigningKey::from_bytes(&[41; 32]);
    let signature = STANDARD.encode(signing.sign(archive).to_bytes());
    let feed = Url::parse("https://example.invalid/release/appcast.xml")?;
    let xml = format!(
        r#"<rss xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle"><channel>
    <item><sparkle:version>202611182201</sparkle:version><sparkle:minimumSystemVersion>14.0</sparkle:minimumSystemVersion>
      <enclosure url="pi.dmg" length="{}" sparkle:edSignature="{signature}" /></item>
    <item><sparkle:version>202611182300</sparkle:version><sparkle:minimumSystemVersion>15.0</sparkle:minimumSystemVersion>
      <enclosure url="newer.dmg" length="{}" sparkle:edSignature="{signature}" /></item>
    <item><enclosure url="legacy.dmg" length="{}" sparkle:version="202611182200" sparkle:edSignature="{signature}" /></item>
    </channel></rss>"#,
        archive.len(),
        archive.len(),
        archive.len()
    );
    let all = updater::parse_appcast(&xml, &feed)?;
    let selected = updater::select(&all, "202611182200", Some("14.4.0"), "arm64")?
        .ok_or("missing compatible update")?;
    assert_eq!(selected.version, "202611182201");
    assert_eq!(
        selected.url.as_str(),
        "https://example.invalid/release/pi.dmg"
    );
    let key = STANDARD.encode(signing.verifying_key().as_bytes());
    updater::verify_archive(&selected, &key, archive)?;
    let mut tampered = archive.to_vec();
    tampered[0] ^= 1;
    assert!(updater::verify_archive(&selected, &key, &tampered).is_err());
    assert!(updater::verify_archive(&selected, &key, &archive[..8]).is_err());
    assert!(updater::select(&all, "202611182300", Some("15.0"), "arm64")?.is_none());
    assert!(
        updater::parse_appcast(
            "<!DOCTYPE rss [<!ENTITY x SYSTEM 'file:///passwd'>]><rss><channel>&x;</channel></rss>",
            &feed
        )
        .is_err()
    );
    Ok(())
}
