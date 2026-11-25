use pi_rs_tui::terminal_image::{
    ImageProtocol, Iterm2Options, ItermSize, KittyImageMetadata, KittyOptions,
    crop_kitty_image_line, detect_capabilities_with, encode_iterm2, encode_kitty,
    get_image_line_extent, get_kitty_image_placement, register_kitty_image_metadata,
    strip_image_sequences,
};

#[test]
fn kitty_repositions_chunked_images_without_retransmitting_pixels() -> Result<(), &'static str> {
    let data = "A".repeat(4100);
    let id = 42;
    let image = encode_kitty(
        &data,
        KittyOptions {
            columns: Some(4),
            rows: Some(3),
            image_id: Some(id),
            move_cursor: Some(false),
        },
    );
    assert!(image.starts_with("\x1b_Ga=T,f=100,q=2,C=1,c=4,r=3,i=42,m=1;"));
    assert!(image.ends_with("\x1b_Gm=0;AAAA\x1b\\"));
    register_kitty_image_metadata(KittyImageMetadata {
        image_id: id,
        columns: 4,
        rows: 3,
        width_px: 80,
        height_px: 50,
    });
    let line = format!("  {image}  ");
    let placement = get_kitty_image_placement(&line).ok_or("registered image has no placement")?;
    assert_eq!(
        placement.replacement_line,
        "  \x1b_Ga=p,q=2,C=1,c=4,r=3,i=42\x1b\\  "
    );
    assert_eq!(placement.transmission_bytes, image.len());
    assert!(crop_kitty_image_line(&line, 1, 1).contains(",y=16,h=18,r=1;"));
    assert!(
        get_image_line_extent(&line)
            .is_some_and(|e| { e.start_col == 2 && e.end_col == Some(6) && e.last_row == 2 })
    );
    Ok(())
}

#[test]
fn iterm_image_extent_does_not_count_its_cursor_up_sequence() {
    let image = encode_iterm2(
        "TQ==",
        Iterm2Options {
            width: Some(ItermSize::Cells(5)),
            height: Some(ItermSize::Value("auto")),
            ..Default::default()
        },
    );
    assert_eq!(
        image,
        "\x1b]1337;File=inline=1;size=1;width=5;height=auto:TQ==\x07"
    );
    let line = format!("中\x1b[2A{image}");
    assert!(get_image_line_extent(&line).is_some_and(|e| {
        e.first_row == -2 && e.last_row == 0 && e.start_col == 2 && e.end_col == Some(7)
    }));
    assert_eq!(strip_image_sequences(&line), "中");
}

#[test]
fn multiplexer_detection_beats_leaked_terminal_identity() {
    let tmux = detect_capabilities_with(
        |key| match key {
            "TERM_PROGRAM" => Some("kitty".into()),
            "TMUX" => Some("/tmp/tmux".into()),
            "PI_HYPERLINKS" => Some("0".into()),
            _ => None,
        },
        false,
        || true,
    );
    assert_eq!(tmux.images, None);
    assert!(!tmux.hyperlinks);
    let standalone = detect_capabilities_with(
        |key| match key {
            "TERM_PROGRAM" => Some("herdr".into()),
            "KITTY_WINDOW_ID" => Some("99".into()),
            _ => None,
        },
        false,
        || false,
    );
    assert_eq!(standalone.images, Some(ImageProtocol::Kitty));
    assert!(standalone.hyperlinks);
    assert!(!standalone.true_color);
}
