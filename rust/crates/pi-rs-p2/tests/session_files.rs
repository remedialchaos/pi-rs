use pi_rs_chord::node::app::{AppOptions, start_server};
use pi_rs_p2::runtime::{
    files::{file_preview, safe_file_name},
    session_files::*,
};
use serde_json::json;
use std::future::ready;

#[test]
fn preview_and_download_names_match_display_contract() {
    assert_eq!(safe_file_name("../.Ａ ①.md"), "A_1.md");
    assert_eq!(safe_file_name("..."), "file");
    assert_eq!(safe_file_name("aⅧb"), "aVIIIb");
    for (count, prefix) in [(118, "𐐀"), (119, "�"), (120, "")] {
        let input = format!("𐐀{}", "a".repeat(count));
        assert_eq!(
            safe_file_name(&input),
            format!("{prefix}{}", "a".repeat(count))
        );
    }
    assert_eq!(safe_file_name(&"a".repeat(121)).len(), 120);
    assert_eq!(file_preview("PNG").map(|p| p.mime_type), Some("image/png"));
    assert!(file_preview("x.SVG").is_none());
}

#[tokio::test]
async fn snapshot_boundaries_and_failed_upload_cleanup() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let app = start_server(AppOptions::new(
        "example",
        temp.path().join("data"),
        json!({"kind":"owner"}),
        json!({"kind":"server"}),
    ))
    .await?;
    let files = &app.files;
    for size in [0, MAX_DOWNLOAD_BYTES as usize] {
        let mut body = Some(vec![42; size]);
        let info = save_download(files, "../a b", size as u64, "worker", None, || {
            ready(Ok(body.take().unwrap_or_default()))
        })
        .await?;
        let (file, name) = find_download(files, &info.id).ok_or("missing snapshot")?;
        assert_eq!(name, "a_b");
        assert_eq!(read_whole(files, &file)?, vec![42; size]);
        assert_eq!(info.size, size as u64);
    }
    let mut chunks = [vec![1], vec![2]].into_iter();
    let result = write_chunks(
        files,
        FileWrite::new(UPLOADS, "partial"),
        || ready(Ok(chunks.next().unwrap_or_default())),
        1,
    )
    .await;
    assert!(matches!(result, Err(DownloadError::TooLarge(_))));
    assert_eq!(
        std::fs::read_dir(files.directory().join(".uploads"))?.count(),
        0
    );
    assert!(files.get(UPLOADS, "partial", None)?.is_none());
    assert!(find_download(files, "missing").is_none());
    Ok(())
}
