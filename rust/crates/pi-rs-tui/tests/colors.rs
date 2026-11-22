//! The tests of `packages/tui/test/colors.test.ts` and of the terminal colors.

use pi_rs_tui::colors::{
    Color, ColorMixSpace, OkhslChannels, OklchColorValue, RgbColorValue, TerminalColorMode,
    TextAttributes, TextStyle, background_ansi, color_to_hex, color_to_okhsl, color_to_oklch,
    color_to_rgb, foreground_ansi, indexed_color, mix_colors, okhsl_color, oklch_color,
    parse_color, rgb_color, style_text, style_text_with_ansi,
};
use pi_rs_tui::oklab;
use pi_rs_tui::terminal_colors::{
    OscColorResponse, OscColorTarget, RgbColor, TerminalColorScheme, parse_osc_color_response,
    parse_terminal_color_scheme_report,
};

fn rgb(r: f64, g: f64, b: f64) -> RgbColorValue {
    RgbColorValue { r, g, b }
}

fn close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn parses_hex_and_oklch_colors_and_rejects_everything_else() {
    assert_eq!(
        parse_color("#abc").unwrap(),
        Color::Rgb(rgb(170.0, 187.0, 204.0))
    );
    assert_eq!(
        parse_color("oklch(62% 0.1 200)").unwrap(),
        Color::Oklch(OklchColorValue {
            l: 0.62,
            c: 0.1,
            h: 200.0
        })
    );
    assert!(parse_color("").unwrap_err().contains("Invalid color value"));
    assert!(
        parse_color("red")
            .unwrap_err()
            .contains("Invalid color value")
    );
}

#[test]
fn gamut_maps_oklch_to_srgb_including_the_lightness_limits() {
    assert_eq!(
        color_to_rgb(&oklch_color(0.627955, 0.257683, 29.2339).unwrap()),
        rgb(255.0, 0.0, 0.0)
    );
    assert_eq!(
        color_to_rgb(&oklch_color(1.0, 0.3, 150.0).unwrap()),
        rgb(255.0, 255.0, 255.0)
    );
    assert_eq!(
        color_to_rgb(&oklch_color(0.0, 0.3, 150.0).unwrap()),
        rgb(0.0, 0.0, 0.0)
    );
}

#[test]
fn parses_okhsl_colors_and_round_trips_them() {
    // Full saturation at the red cusp is pure sRGB red.
    assert_eq!(
        parse_color("okhsl(29.23 100% 56.8%)").unwrap(),
        rgb_color(255.0, 0.0, 0.0).unwrap()
    );
    assert_eq!(
        parse_color("OKHSL(250deg 60% 55%)").unwrap(),
        okhsl_color(250.0, 0.6, 0.55).unwrap()
    );
    assert!(
        parse_color("okhsl(250 160% 55%)")
            .unwrap_err()
            .contains("s must be between 0 and 1")
    );
    for hex in ["#4f8eb3", "#20242a", "#f8f9fa"] {
        let color = parse_color(hex).unwrap();
        let OkhslChannels { h, s, l } = color_to_okhsl(&color);
        assert_eq!(color_to_hex(&okhsl_color(h, s, l).unwrap()), hex);
    }
}

#[test]
fn styles_text_and_closes_sequences_in_reverse_order() {
    let style = TextStyle {
        attributes: TextAttributes {
            bold: true,
            italic: true,
            ..Default::default()
        },
        fg: Some(rgb_color(18.0, 52.0, 86.0).unwrap()),
        bg: Some(indexed_color(9.0).unwrap()),
    };
    assert_eq!(
        style_text("Ready", &style, TerminalColorMode::Truecolor),
        "\x1b[38;2;18;52;86m\x1b[48;5;9m\x1b[1m\x1b[3mReady\x1b[23m\x1b[22m\x1b[49m\x1b[39m"
    );
    let style = TextStyle {
        attributes: TextAttributes::default(),
        fg: Some(rgb_color(18.0, 52.0, 86.0).unwrap()),
        bg: None,
    };
    let output = style_text("Ready", &style, TerminalColorMode::Color256);
    let prefix = "\x1b[38;5;";
    assert!(
        output.starts_with(prefix),
        "unexpected escape sequence: {output:?}"
    );
    let rest = &output[prefix.len()..];
    let digits = rest
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .count();
    assert!(digits > 0, "unexpected escape sequence: {output:?}");
    assert_eq!(&rest[digits..], "mReady\x1b[39m");
}

#[test]
fn styles_text_with_precomputed_escape_sequences() {
    let attributes = TextAttributes {
        underline: true,
        inverse: true,
        strikethrough: true,
        ..Default::default()
    };
    assert_eq!(
        style_text_with_ansi(
            "Ready",
            Some("\x1b[38;2;18;52;86m"),
            Some("\x1b[48;5;9m"),
            &attributes,
        ),
        "\x1b[38;2;18;52;86m\x1b[48;5;9m\x1b[4m\x1b[7m\x1b[9mReady\x1b[29m\x1b[27m\x1b[24m\x1b[49m\x1b[39m"
    );
    assert_eq!(
        style_text_with_ansi("Ready", None, None, &TextAttributes::default()),
        "Ready"
    );
}

#[test]
fn rejects_the_colors_that_are_out_of_range() {
    assert_eq!(
        rgb_color(f64::NAN, 0.0, 0.0).unwrap_err(),
        "r must be finite"
    );
    assert_eq!(
        rgb_color(0.0, f64::INFINITY, 0.0).unwrap_err(),
        "g must be finite"
    );
    assert_eq!(
        rgb_color(0.0, 0.0, 256.0).unwrap_err(),
        "b must be between 0 and 255: 256"
    );
    assert_eq!(
        indexed_color(256.0).unwrap_err(),
        "ANSI color index must be an integer from 0 to 255: 256"
    );
    assert_eq!(
        indexed_color(1.5).unwrap_err(),
        "ANSI color index must be an integer from 0 to 255: 1.5"
    );
    assert_eq!(
        oklch_color(1.5, 0.0, 0.0).unwrap_err(),
        "l must be between 0 and 1: 1.5"
    );
    assert_eq!(
        oklch_color(0.5, -1.0, 0.0).unwrap_err(),
        "c must not be negative: -1"
    );
}

#[test]
fn mixes_the_colors_in_the_oklch_and_the_srgb_space() {
    let first = parse_color("#ff0000").unwrap();
    let second = parse_color("#0000ff").unwrap();
    let mixed = mix_colors(&first, &second, 0.5, ColorMixSpace::Srgb).unwrap();
    assert_eq!(color_to_rgb(&mixed), rgb(127.5, 0.0, 127.5));

    let mixed = mix_colors(&first, &second, 0.5, ColorMixSpace::Oklch).unwrap();
    let channels = color_to_oklch(&mixed);
    close(channels.l, 0.5399845410479271);
    close(channels.c, 0.2854488462199227);
    close(channels.h, 326.6429514479989);
}

#[test]
fn converts_between_the_color_spaces() {
    let color = parse_color("#4f8eb3").unwrap();
    let lab = oklab::rgb_to_oklab(&color_to_rgb(&color));
    close(lab[0], 0.6196140681598568);
    close(lab[1], -0.047762195757293846);
    close(lab[2], -0.07133871126891644);

    let linear = oklab::oklab_to_linear_srgb(lab);
    let back = oklab::linear_srgb_to_rgb(linear);
    close(back.r, 79.0);
    close(back.g, 142.0);
    close(back.b, 179.0);

    let lightness = oklab::oklab_to_okhsl_lightness(lab[0]);
    close(lightness, 0.5584624058913696);
}

#[test]
fn maps_the_palette_to_the_srgb_colors() {
    for (index, hex) in [
        (0.0, "#000000"),
        (1.0, "#800000"),
        (7.0, "#c0c0c0"),
        (8.0, "#808080"),
        (9.0, "#ff0000"),
        (15.0, "#ffffff"),
        (16.0, "#000000"),
        (17.0, "#00005f"),
        (51.0, "#00ffff"),
        (231.0, "#ffffff"),
        (232.0, "#080808"),
        (233.0, "#121212"),
        (255.0, "#eeeeee"),
    ] {
        let color = indexed_color(index).unwrap();
        assert_eq!(color_to_hex(&color), hex, "palette index {index}");
        assert_eq!(
            foreground_ansi(&color, TerminalColorMode::Truecolor),
            format!("\x1b[38;5;{}m", index as i64)
        );
        assert_eq!(
            background_ansi(&color, TerminalColorMode::Color256),
            format!("\x1b[48;5;{}m", index as i64)
        );
    }
}

#[test]
fn approximates_the_colors_with_the_256_color_palette() {
    for (r, g, b, index) in [
        (0.0, 0.0, 0.0, 16),
        (255.0, 255.0, 255.0, 231),
        (18.0, 52.0, 86.0, 23),
        (128.0, 128.0, 128.0, 244),
        (1.0, 2.0, 3.0, 16),
        (250.0, 5.0, 120.0, 198),
        (100.0, 100.0, 100.0, 241),
        (96.0, 96.0, 96.0, 59),
        (200.0, 100.0, 50.0, 167),
    ] {
        let color = rgb_color(r, g, b).unwrap();
        assert_eq!(
            foreground_ansi(&color, TerminalColorMode::Color256),
            format!("\x1b[38;5;{index}m"),
            "the color {r} {g} {b}"
        );
        assert_eq!(
            foreground_ansi(&color, TerminalColorMode::Truecolor),
            format!("\x1b[38;2;{};{};{}m", r as i64, g as i64, b as i64)
        );
    }
}

#[test]
fn parses_the_color_scheme_reports() {
    assert_eq!(
        parse_terminal_color_scheme_report("\x1b[?997;1n"),
        Some(TerminalColorScheme::Dark)
    );
    assert_eq!(
        parse_terminal_color_scheme_report("\x1b[?997;2n"),
        Some(TerminalColorScheme::Light)
    );
    assert_eq!(
        parse_terminal_color_scheme_report("\x1b[?997;2n\x1b[?997;1n\x1b[?997;1n"),
        Some(TerminalColorScheme::Dark)
    );
    assert_eq!(
        parse_terminal_color_scheme_report("\x1b[?997;1n\x1b[?997;2n\x1b[?997;2n"),
        Some(TerminalColorScheme::Light)
    );
    assert_eq!(parse_terminal_color_scheme_report("\x1b[?997;3n"), None);
    assert_eq!(parse_terminal_color_scheme_report("\x1b[996n"), None);
    assert_eq!(parse_terminal_color_scheme_report("x\x1b[?997;1n"), None);
}

#[test]
fn parses_the_osc_10_11_and_4_replies() {
    assert_eq!(
        parse_osc_color_response("\x1b]10;rgb:ffff/ffff/ffff\x07"),
        Some(OscColorResponse {
            target: OscColorTarget::Foreground,
            rgb: Some(RgbColor {
                r: 255.0,
                g: 255.0,
                b: 255.0
            }),
        })
    );
    assert_eq!(
        parse_osc_color_response("\x1b]4;13;#ff0080\x1b\\"),
        Some(OscColorResponse {
            target: OscColorTarget::Index(13),
            rgb: Some(RgbColor {
                r: 255.0,
                g: 0.0,
                b: 128.0
            }),
        })
    );
    assert_eq!(
        parse_osc_color_response("\x1b]4;1;bogus\x07"),
        Some(OscColorResponse {
            target: OscColorTarget::Index(1),
            rgb: None,
        })
    );
    assert_eq!(parse_osc_color_response("\x1b]12;#ffffff\x07"), None);
}

#[test]
fn parses_the_osc_replies_of_every_form() {
    let white = RgbColor {
        r: 255.0,
        g: 255.0,
        b: 255.0,
    };
    assert_eq!(
        parse_osc_color_response("\x1b]11;#ffffff\x07").unwrap().rgb,
        Some(white)
    );
    assert_eq!(
        parse_osc_color_response("\x1b]11;rgb:0000/0000/0000\x1b\\")
            .unwrap()
            .rgb,
        Some(RgbColor {
            r: 0.0,
            g: 0.0,
            b: 0.0
        })
    );
    assert_eq!(
        parse_osc_color_response("\x1b]4;0;#ffffffffffff\x07")
            .unwrap()
            .rgb,
        Some(white)
    );
    assert_eq!(
        parse_osc_color_response("\x1b]4;0;rgb:0000/0000/0000\x07")
            .unwrap()
            .rgb,
        Some(RgbColor {
            r: 0.0,
            g: 0.0,
            b: 0.0
        })
    );
    assert_eq!(
        parse_osc_color_response("\x1b]4;0;rgb:ff/00/00\x07")
            .unwrap()
            .rgb,
        Some(RgbColor {
            r: 255.0,
            g: 0.0,
            b: 0.0
        })
    );
}
