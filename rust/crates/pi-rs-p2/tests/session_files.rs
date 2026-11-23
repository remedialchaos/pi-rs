use pi_rs_chord::node::app::{AppOptions, AppServer, Files, start_server};
use pi_rs_p2::runtime::files::{FilePreview, file_preview, safe_file_name};
use pi_rs_p2::runtime::session_files::{
    self, DOWNLOADS, DownloadError, FILE_CHUNK, FileInfo, FileWrite, MAX_DOWNLOAD_BYTES, UPLOADS,
};
use serde_json::json;
use std::future::ready;
use tempfile::TempDir;

/// A `next` reader over fixed chunks: yields each slice once, then empty (the end of the stream).
fn reader(chunks: Vec<Vec<u8>>) -> impl FnMut() -> std::future::Ready<std::io::Result<Vec<u8>>> {
    let mut rest = chunks;
    move || {
        ready(Ok(if rest.is_empty() {
            Vec::new()
        } else {
            rest.remove(0)
        }))
    }
}

/// A minimal `FileInfo` for calling the read paths directly.
fn info(id: &str, path: &str, size: u64) -> FileInfo {
    FileInfo {
        id: id.into(),
        space: DOWNLOADS.into(),
        path: path.into(),
        version: 1,
        size,
        mime: "application/octet-stream".into(),
        source: "agent".into(),
        reference: None,
        key: None,
        created: 0,
        meta: None,
    }
}

/// A session worker's `Files` on its own throwaway data directory.
async fn session_files() -> Result<(Files, TempDir, AppServer), pi_rs_chord::Error> {
    let directory = TempDir::new()?;
    let app = start_server(AppOptions::new(
        "example",
        directory.path().join("data"),
        json!({"kind":"owner"}),
        json!({"kind":"server"}),
    ))
    .await?;
    Ok((app.files.clone(), directory, app))
}

/// The recorded upstream display-name contract, including its Unicode edge cases (the oracles).
#[test]
fn safe_file_name_matches_upstream_sanitization() {
    assert_eq!(safe_file_name("notes.txt"), "notes.txt");
    // Path segments never survive: only the last segment is used, `/` and `\` alike.
    assert_eq!(safe_file_name("../../evil notes.txt"), "evil_notes.txt");
    assert_eq!(safe_file_name(r"C:\Users\k\report(2).png"), "report_2_.png");
    // Runs of disallowed characters collapse into one `_`.
    assert_eq!(safe_file_name("a b.txt"), "a_b.txt");
    assert_eq!(
        safe_file_name("a  b.txt"),
        "a_b.txt",
        "one underscore per run"
    );
    // NFKC compatibility folding before the filter (fullwidth, circled digit, Roman numeral).
    assert_eq!(safe_file_name("../.Ａ ①.md"), "A_1.md");
    assert_eq!(safe_file_name("aⅧb"), "aVIIIb");
    // Combining marks (NFKC-stable) are one replacement run each.
    assert_eq!(safe_file_name("a\u{345}b"), "a_b");
    assert_eq!(safe_file_name("a\u{5b0}b"), "a_b");
    // A dot-prefixed name loses its leading dots; empty (and all-dot) names fall back to `file`.
    assert_eq!(safe_file_name(".hidden"), "hidden");
    assert_eq!(safe_file_name("..."), "file");
    assert_eq!(
        safe_file_name("   "),
        "_",
        "a run of spaces is one underscore"
    );
    assert_eq!(safe_file_name(""), "file");
    // The slice is in UTF-16 code units; when it cuts a surrogate pair the low half stays alone
    // (the JS string keeps \uDC00; ours keeps U+FFFD), and at 121 units the lead character drops.
    for (count, prefix) in [(118, "𐐀"), (119, "\u{FFFD}"), (120, "")] {
        let input = format!("𐐀{}", "a".repeat(count));
        assert_eq!(
            safe_file_name(&input),
            format!("{prefix}{}", "a".repeat(count))
        );
    }
    let plain = safe_file_name(&"a".repeat(121));
    assert_eq!(plain, "a".repeat(120));
    assert_eq!(plain.encode_utf16().count(), 120);
    // A lone low surrogate (however it arrives) is not a letter or number: one `_`, then the cut.
    assert_eq!(
        safe_file_name(&format!("\u{FFFD}{}", "a".repeat(120))),
        "a".repeat(120)
    );
}

/// `filePreview` is a conservative extension allowlist, matched case-insensitively on the last
/// dot-segment; unknown or active formats (SVG) stay unknown so they are served for download.
#[test]
fn file_preview_allowlist_matches_upstream() {
    fn preview(name: &str) -> Option<(String, String)> {
        file_preview(name).map(|p| (p.kind.to_string(), p.mime_type.to_string()))
    }
    let text = ("text".into(), "text/plain; charset=utf-8".into());
    assert_eq!(preview("a.txt"), Some(text.clone()));
    assert_eq!(preview("a.MD"), Some(text.clone()));
    assert_eq!(preview("a.markdown"), Some(text.clone()));
    assert_eq!(
        preview("notes.tar.md"),
        Some(text.clone()),
        "the last segment wins"
    );
    assert_eq!(
        preview("dot.png"),
        Some(("image".into(), "image/png".into()))
    );
    assert_eq!(
        preview("photo.JPEG"),
        Some(("image".into(), "image/jpeg".into()))
    );
    assert_eq!(
        preview("clip.M4A"),
        Some(("audio".into(), "audio/mp4".into()))
    );
    assert_eq!(
        preview("movie.WEBM"),
        Some(("video".into(), "video/webm".into()))
    );
    assert_eq!(preview("x.svg"), None, "active formats never preview");
    assert_eq!(preview("x.exe"), None);
    assert_eq!(preview("archive.tar.gz"), None);
    assert_eq!(preview("noextension"), None);
    // The `FilePreview` value serializes exactly like the upstream wire object.
    assert_eq!(
        serde_json::to_value(FilePreview {
            kind: "image",
            mime_type: "image/png"
        })
        .ok(),
        Some(json!({"kind":"image","mimeType":"image/png"}))
    );
}

/// `display_name`: `meta.name` as p2 writes it, else the path's last segment.
#[test]
fn display_name_prefers_meta_name_else_last_path_segment() {
    let with_meta = FileInfo {
        meta: Some(json!({"name":"résumé_.txt"})),
        ..info("1", "id-report.txt", 0)
    };
    assert_eq!(session_files::display_name(&with_meta), "résumé_.txt");
    let without_meta = FileInfo {
        meta: None,
        ..with_meta
    };
    assert_eq!(session_files::display_name(&without_meta), "id-report.txt");
    let non_string = FileInfo {
        meta: Some(json!({"name": 3})),
        ..without_meta
    };
    assert_eq!(session_files::display_name(&non_string), "id-report.txt");
    let top_level = FileInfo {
        path: "plain".into(),
        ..non_string
    };
    assert_eq!(session_files::display_name(&top_level), "plain");
}

/// Snapshots round-trip through the `downloads` space: sanitized display names in `meta`, the id
/// as the version's key, whole reads back at every size boundary, and unknown or empty ids denied.
#[tokio::test]
async fn snapshots_round_trip_with_sanitized_names() -> Result<(), Box<dyn std::error::Error>> {
    let (files, _directory, _app) = session_files().await?;
    for (size, name) in [
        (0, "../a b"),
        (FILE_CHUNK + 5, "report(1).txt"),
        (MAX_DOWNLOAD_BYTES as usize, ".dot"),
    ] {
        let info = session_files::save_download(
            &files,
            name,
            size as u64,
            "agent",
            Some("offered-by".into()),
            reader(vec![vec![7; size]]),
        )
        .await?;
        let expected_name = match name {
            ".dot" => "dot",
            "../a b" => "a_b",
            _ => "report_1_.txt",
        };
        assert_eq!(info.name, expected_name);
        let (file, display) = session_files::find_download(&files, &info.id)
            .ok_or("snapshot missing from the downloads space")?;
        assert_eq!(display, expected_name);
        assert_eq!(file.space, DOWNLOADS);
        assert_eq!(file.key.as_deref(), Some(info.id.as_str()));
        assert_eq!(file.path, format!("{}-{}", info.id, expected_name));
        assert_eq!(file.source, "agent");
        assert_eq!(file.reference.as_deref(), Some("offered-by"));
        assert_eq!(info.size, size as u64);
        assert_eq!(session_files::read_whole(&files, &file)?, vec![7; size]);
    }
    // Denials: an empty id and an id that was never saved are both simply not found.
    assert!(session_files::find_download(&files, "").is_none());
    assert!(session_files::find_download(&files, "missing").is_none());
    Ok(())
}

/// `write_chunks` splits pieces larger than `FILE_CHUNK` into store-sized appends, stops at the
/// first empty piece, and reuses an existing version when the write's key already has one.
#[tokio::test]
async fn write_chunks_splits_pieces_and_reuses_keys() -> Result<(), Box<dyn std::error::Error>> {
    let (files, _directory, _app) = session_files().await?;
    // One 2.5 MiB piece from the source becomes FILE_CHUNK-sized appends under a 3 MiB cap.
    let body: Vec<u8> = (0..FILE_CHUNK * 2 + 5).map(|i| (i % 251) as u8).collect();
    let file = session_files::write_chunks(
        &files,
        FileWrite::new(UPLOADS, "big"),
        reader(vec![body.clone()]),
        (FILE_CHUNK * 3) as u64,
    )
    .await?;
    assert_eq!(file.size as usize, body.len());
    let read = session_files::read_whole(&files, &file)?;
    assert_eq!(read.len(), body.len());
    assert_eq!(&read[..FILE_CHUNK], &body[..FILE_CHUNK]);
    assert_eq!(&read[FILE_CHUNK..], &body[FILE_CHUNK..]);
    // An empty first piece writes an empty version.
    let empty =
        session_files::write_chunks(&files, FileWrite::new(UPLOADS, "empty"), reader(vec![]), 10)
            .await?;
    assert_eq!(empty.size, 0);
    assert_eq!(session_files::read_whole(&files, &empty)?, Vec::<u8>::new());
    // A write whose key already exists returns the stored version without reading the source.
    let mut keyed = FileWrite::new(UPLOADS, "keyed");
    keyed.key = Some("the-key".into());
    let first =
        session_files::write_chunks(&files, keyed.clone(), reader(vec![b"x".to_vec()]), 10).await?;
    let again = session_files::write_chunks(
        &files,
        keyed,
        || ready(Err(std::io::Error::other("the source must not be read"))),
        10,
    )
    .await?;
    assert_eq!(again.id, first.id);
    assert_eq!(again.size, 1);
    Ok(())
}

/// Oversized sources are refused with the 413 `TooLarge` denial, before any bytes are stored, and
/// a failed upload leaves nothing behind: no staging file, no version, no partial blob.
#[tokio::test]
async fn oversized_and_failed_uploads_are_dropped() -> Result<(), Box<dyn std::error::Error>> {
    let (files, _directory, _app) = session_files().await?;
    // Past the cap mid-stream: the refusal comes before the second piece is appended.
    let result = session_files::write_chunks(
        &files,
        FileWrite::new(UPLOADS, "partial"),
        reader(vec![vec![1], vec![2]]),
        1,
    )
    .await;
    match result {
        Err(DownloadError::TooLarge(mib)) => {
            assert_eq!(mib, 1.0 / 1048576.0, "`max` is expressed in MiB");
            assert_eq!(DownloadError::TooLarge(mib).status(), Some(413));
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_dir(files.directory().join(".uploads"))?.count(),
        0,
        "the failed upload's staging file is gone"
    );
    assert!(files.get(UPLOADS, "partial", None)?.is_none());
    // A snapshot declared larger than 25 MiB never starts writing.
    let refused = session_files::save_download(
        &files,
        "huge.bin",
        MAX_DOWNLOAD_BYTES + 1,
        "agent",
        None,
        reader(vec![vec![0; 1024]]),
    )
    .await;
    assert!(matches!(refused, Err(DownloadError::TooLarge(25.0))));
    assert_eq!(
        std::fs::read_dir(files.directory().join(".uploads"))?.count(),
        0
    );
    // A failing source (here: an I/O error) also drops the begun upload.
    let failing = session_files::write_chunks(
        &files,
        FileWrite::new(UPLOADS, "failing"),
        || ready(Err(std::io::Error::other("source broke"))),
        1024,
    )
    .await;
    assert!(matches!(failing, Err(DownloadError::Reader(_))));
    assert_eq!(
        std::fs::read_dir(files.directory().join(".uploads"))?.count(),
        0
    );
    assert!(files.get(UPLOADS, "failing", None)?.is_none());
    // Reading a version that no longer exists is a store error, not an empty success.
    match session_files::read_whole(&files, &info("missing", "missing.txt", 1)) {
        Err(DownloadError::Store(_)) => {}
        other => panic!("expected Store, got {other:?}"),
    }
    Ok(())
}

/// `read_whole` refuses a version that changed (shrank to nothing) while being read, and only
/// `TooLarge` carries an HTTP status.
#[tokio::test]
async fn read_whole_reports_changed_files() -> Result<(), Box<dyn std::error::Error>> {
    let (files, _directory, _app) = session_files().await?;
    let saved = session_files::save_download(
        &files,
        "vanish.txt",
        4,
        "agent",
        None,
        reader(vec![b"abcd".to_vec()]),
    )
    .await?;
    let (file, _) = session_files::find_download(&files, &saved.id).ok_or("snapshot missing")?;
    assert_eq!(session_files::read_whole(&files, &file)?, b"abcd".to_vec());
    // Lie about the size: the store returns nothing where more bytes were promised.
    let mut changed = file.clone();
    changed.size += 4;
    match session_files::read_whole(&files, &changed) {
        Err(DownloadError::Changed(path)) => assert_eq!(path, file.path),
        other => panic!("expected Changed, got {other:?}"),
    }
    assert_eq!(DownloadError::Changed(String::new()).status(), None);
    Ok(())
}
