use std::time::{Duration, Instant};

use pi_rs_tui::keybindings::{BindingKeys, KeybindingsConfig, KeybindingsManager};
use pi_rs_tui::keys::{decode_kitty_printable, matches_key_data, parse_key};
use pi_rs_tui::stdin_buffer::{StdinBuffer, StdinEvent};
use pi_rs_tui::utils::{
    extract_segments, normalize_terminal_output, slice_by_column, slice_with_width,
    truncate_with_ellipsis, visible_width, wrap_text_with_ansi,
};
use pi_rs_tui::wheel_scroll::{WheelScrollAccelerator, WheelScrollLines};
use pi_rs_tui::word_navigation::{find_word_backward, find_word_forward};

#[test]
fn ansi_and_unicode_width_prevent_terminal_auto_wrap() {
    for (text, expected) in [
        ("\x1b[38;5;4mhello\x1b[39m world", 11),
        ("\x1b]8;;https://example.com\x07link\x1b]8;;\x07", 4),
        ("\x1b]133;A\x1b\\prompt", 6),
        ("\x1b_pi:c\x07cursor", 6),
        ("\x1b[1ma\tb\x1b[22m", 5),
        ("\x1b[31m日本\x1b[39m ok", 7),
        ("\x1b[31m─→\x1b[39m", 2),
        ("\x1b[31", 3),
        ("क\u{93e}", 2),
        ("\u{93e}", 1),
        ("🇨", 2),
        ("🇨🇳", 2),
        ("👨‍💻", 2),
        ("⚡️", 2),
        ("🏳️‍🌈", 2),
    ] {
        assert_eq!(visible_width(text), expected, "{text:?}");
    }
    assert_eq!(wrap_text_with_ansi("      - 🇨", 9), ["      -", "🇨"]);
}

#[test]
fn wrapped_links_preserve_bel_and_survive_sgr_reset() {
    let open = "\x1b]8;;https://example.com\x07";
    let close = "\x1b]8;;\x07";
    let text = format!("{open}\x1b[4mabc\x1b[0mdef{close}");
    assert_eq!(
        wrap_text_with_ansi(&text, 3),
        [
            format!("{open}\x1b[4mabc\x1b[0m{close}"),
            format!("{open}def{close}")
        ],
    );
    assert_eq!(
        truncate_with_ellipsis(&text, 5, "…", false),
        format!("{open}\x1b[4mabc\x1b[0md{close}\x1b[0m…\x1b[0m"),
    );
}

#[test]
fn slicing_keeps_style_order_and_never_copies_old_apc_markers() {
    assert_eq!(
        slice_by_column("\x1b[32mfoo\x1b[39m bar", 3, 4, true),
        "\x1b[32m\x1b[39m bar",
    );
    assert_eq!(slice_by_column("ab\x1b_pi:c\x07cdefghij", 4, 2, true), "ef");
    let text = "out 192M\t.pi/skill-tests/results-ha";
    let slice = slice_with_width(text, 0, 10, true);
    assert_eq!((slice.text.as_str(), slice.width), ("out 192M", 8));
    let result = extract_segments(text, 11, 13, 10, true);
    assert_eq!(
        (result.before.as_str(), result.before_width),
        ("out 192M\t", 11)
    );
    assert_eq!(
        normalize_terminal_output("\x1b]0;window\ttitle\x1b\\label\ttext"),
        "\x1b]0;window\ttitle\x1b\\label   text",
    );
}

#[test]
fn kitty_alternate_keys_keep_logical_and_physical_bindings() {
    let data = "\x1b[61:43;2u";
    assert_eq!(parse_key(data).as_ref().map(|key| key.id()), Some("+"));
    assert_eq!(decode_kitty_printable(data).as_deref(), Some("+"));
    assert!(matches_key_data(data, "+"));
    assert!(matches_key_data(data, "shift+="));
    assert!(matches_key_data("\x1b[1089:1057:99;6:2u", "ctrl+shift+c"));
}

#[test]
fn user_conflicts_do_not_evict_unrelated_default_bindings() {
    let mut bindings = KeybindingsManager::default();
    bindings.set_user_bindings(KeybindingsConfig::from([
        (
            "tui.input.submit".to_owned(),
            BindingKeys::Many(vec!["ctrl+x".into(), "ctrl+x".into()]),
        ),
        (
            "tui.select.confirm".to_owned(),
            BindingKeys::One("ctrl+x".into()),
        ),
    ]));
    assert_eq!(bindings.get_keys("tui.input.submit"), ["ctrl+x"]);
    assert_eq!(
        bindings.get_conflicts()[0].keybindings,
        ["tui.input.submit", "tui.select.confirm"]
    );
    assert!(bindings.matches("\x18", "tui.select.confirm"));
    assert!(bindings.matches("\x1b[D", "tui.editor.cursorLeft"));
}

#[test]
fn delayed_terminal_replies_do_not_turn_into_keyboard_shortcuts() {
    let start = Instant::now();
    let late = start + Duration::from_secs(5);
    let reply = "\x1b]4;1;rgb:eaea/6c6c/6d6d\x07";
    for split in 6..reply.len() {
        let mut input = StdinBuffer::default();
        assert!(input.process(&reply[..split], start).is_empty());
        assert!(input.expire(late).is_empty());
        assert_eq!(
            input.process(&format!("{}x", &reply[split..]), late),
            [StdinEvent::Data(reply.into()), StdinEvent::Data("x".into()),]
        );
    }
    let mut input = StdinBuffer::default();
    assert!(input.process("\x1b]10x\r", start).is_empty());
    assert_eq!(
        input.expire(late),
        ["\x1b]", "1", "0", "x", "\r"].map(|s| StdinEvent::Data(s.into()))
    );
    assert!(input.process("\x1b[200~文\x1b[20", start).is_empty());
    assert_eq!(
        input.process("1~y", late),
        [StdinEvent::Paste("文".into()), StdinEvent::Data("y".into())]
    );
    let ascii = "\x1b[200~ab\x1b[201~c";
    for chunk_size in 1..=ascii.len() {
        let mut buffer = StdinBuffer::default();
        let mut events = Vec::new();
        for chunk in ascii.as_bytes().chunks(chunk_size) {
            events.extend(buffer.process(std::str::from_utf8(chunk).expect("ascii"), start));
        }
        assert_eq!(
            events,
            [StdinEvent::Paste("ab".into()), StdinEvent::Data("c".into())],
            "chunk size {chunk_size}"
        );
    }
    let utf8 = "\x1b[200~a界\x1b[201~c";
    for split in 1..utf8.len() {
        if !utf8.is_char_boundary(split) {
            continue;
        }
        let mut buffer = StdinBuffer::default();
        let mut events = buffer.process(&utf8[..split], start);
        events.extend(buffer.process(&utf8[split..], start));
        assert_eq!(
            events,
            [
                StdinEvent::Paste("a界".into()),
                StdinEvent::Data("c".into())
            ],
            "split at {split}"
        );
    }
}

#[test]
fn wheel_gestures_carry_fractional_lines_but_reset_on_direction_change() {
    let mut wheel = WheelScrollAccelerator::new(WheelScrollLines::Auto, true);
    assert_eq!(
        [0., 40., 80., 120., 160.].map(|time| wheel.next(1, time)),
        [1, 2, 3, 2, 3]
    );
    assert_eq!(wheel.next(-1, 180.), 1);
    assert_eq!(wheel.next(-1, 200.), 5);
    assert_eq!(wheel.next(-1, 203.), 1);
    assert_eq!(wheel.next(-1, 500.), 1);
}

#[test]
fn word_navigation_stops_at_ascii_punctuation_and_cjk_dictionary_boundaries() {
    let word = "foo.bar";
    assert_eq!(
        [0, 3, 4].map(|i| find_word_forward(word, i, None)),
        [3, 4, 7]
    );
    assert_eq!(
        [7, 4, 3].map(|i| find_word_backward(word, i, None)),
        [4, 3, 0]
    );
    let text = "你好世界 test";
    let word = "你好".len();
    assert_eq!(find_word_forward(text, 0, None), word);
    assert_eq!(find_word_backward(text, "你好世界 ".len(), None), word);
}
