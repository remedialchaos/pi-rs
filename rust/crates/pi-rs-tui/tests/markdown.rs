use std::cell::RefCell;
use std::rc::Rc;

use pi_rs_tui::{
    Component, DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme,
    strip_terminal_sequences, visible_width,
};

fn plain(source: &str, width: usize) -> Vec<String> {
    let markdown = Markdown::new(
        source,
        0,
        0,
        MarkdownTheme::default(),
        DefaultTextStyle::default(),
        MarkdownOptions::default(),
    );
    markdown
        .render(width)
        .iter()
        .map(|s| strip_terminal_sequences(s).trim_end().to_owned())
        .collect()
}

#[test]
fn cjk_emphasis_is_recognized_without_relaxing_english_flanking() {
    for (source, expected) in [
        ("**这是测试。**后面", "<b>这是测试。</b>后面"),
        ("**一句话说清：**一台机器", "<b>一句话说清：</b>一台机器"),
        ("**要点到此为止。**20 小时", "<b>要点到此为止。</b>20 小时"),
        ("字**「引用」**字", "字<b>「引用」</b>字"),
        ("*斜体。*后面", "<i>斜体。</i>后面"),
        ("**end.**Then next", "**end.**Then next"),
    ] {
        let theme = MarkdownTheme {
            bold: Rc::new(|text| format!("<b>{text}</b>")),
            italic: Rc::new(|text| format!("<i>{text}</i>")),
            ..MarkdownTheme::default()
        };
        let markdown = Markdown::new(
            source,
            0,
            0,
            theme,
            DefaultTextStyle::default(),
            MarkdownOptions::default(),
        );
        assert_eq!(markdown.render(80)[0].trim_end(), expected, "{source}");
    }
}

#[test]
fn display_math_and_single_dollar_punctuation_keep_their_contract() {
    assert_eq!(
        plain(
            r"A map $\mathbb{C}^3 \to \mathbb{C}^3$, $xy$, $x-y$, $-x$, $\frac{1}{2}$, $\rightarrow$, and \(s \to \infty\).",
            80
        ),
        ["A map ℂ³ → ℂ³, xy, x-y, -x, 1/2, →, and s → ∞."]
    );
    assert_eq!(
        plain(
            "Costs $5 and $10 or $8k–$12k; use `$x$`, $HOME, and ${PATH}.",
            80
        ),
        ["Costs $5 and $10 or $8k–$12k; use $x$, $HOME, and ${PATH}."]
    );
    assert_eq!(
        plain(
            "Before\n\n\\[\nE \\approx \\frac{0.1\\ \\text{lux}}{100\\ \\text{lm/W}}\n\\]\n\nafter",
            80
        ),
        [
            "Before",
            "",
            "    0.1 lux",
            "E ≈ ────────",
            "    100 lm/W",
            "",
            "after"
        ]
    );
    assert_eq!(plain("\\[\nx^2", 80), ["\\[", "x^2"]);
    assert_eq!(plain(r"Map \(\mathbb{C}^3", 80), [r"Map \(\mathbb{C}^3"]);
    assert_eq!(plain(r"Escaped \$x-y\$.", 80), ["Escaped $x-y$."]);
}

#[test]
fn an_unclosed_math_block_does_not_swallow_the_table() {
    let lines = plain(
        "\\[\nx^2\n\n| Value |\n| --- |\n| $\\mathbb{C}^3$ |\n\n$$ y^2 $$",
        60,
    );
    assert!(lines.iter().any(|line| line.contains("│ ℂ³")), "{lines:?}");
    assert!(lines.iter().any(|line| line == "y²"), "{lines:?}");
}

#[test]
fn highlighting_is_reused_on_resize_and_streaming_but_not_invalidation() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let observed = calls.clone();
    let theme = MarkdownTheme {
        highlight_code: Some(Rc::new(move |code, lang| {
            observed
                .borrow_mut()
                .push((lang.unwrap_or("").to_owned(), code.to_owned()));
            code.lines()
                .map(|line| format!("\x1b[32m{line}\x1b[39m"))
                .collect()
        })),
        ..MarkdownTheme::default()
    };
    let mut markdown = Markdown::new(
        "```ts\nx\n```",
        0,
        0,
        theme,
        DefaultTextStyle::default(),
        MarkdownOptions::default(),
    );
    markdown.render(80);
    markdown.set_text("```ts\nx\n```\n\nafter");
    markdown.render(40);
    assert_eq!(*calls.borrow(), [("ts".into(), "x".into())]);
    markdown.invalidate();
    markdown.render(40);
    assert_eq!(
        *calls.borrow(),
        [("ts".into(), "x".into()), ("ts".into(), "x".into())]
    );
}

#[test]
fn lists_quotes_and_partial_fences_keep_continuation_geometry() {
    assert_eq!(
        plain("1. alpha\n1. beta\n   - nested\n   - another\n1. gamma", 80),
        [
            "1. alpha",
            "2. beta",
            "    - nested",
            "    - another",
            "3. gamma"
        ]
    );
    assert_eq!(
        plain("```ts\nconst x = 1;\n``", 80),
        ["```ts", "  const x = 1;", "```"]
    );
    assert_eq!(plain("```ts\n``", 80), ["```ts", "", "```"]);
    assert_eq!(plain("# Title\nfoo", 80), ["Title", "", "foo"]);
    assert_eq!(
        plain("> q\n>\n> <!-- c -->\n>\n> r", 80),
        plain("> q\n>\n> r", 80)
    );
    let lines = plain("- wrapping the words of an unordered list", 16);
    assert_eq!(lines[0], "- wrapping the");
    assert!(lines[1..].iter().all(|line| line.starts_with("  ")));
}

#[test]
fn comments_are_hidden_and_do_not_add_blank_rows() {
    assert_eq!(
        plain(
            "<!-- start -->\n\n## Notes\n\n<!-- a -->\n\n- item\n\n<!-- b -->\n\n## Next\n\n<!-- end -->",
            60
        ),
        plain("## Notes\n\n- item\n\n## Next", 60)
    );
    assert_eq!(
        plain("See <!-- todo: link --> the docs.", 60),
        ["See the docs."]
    );
    assert_eq!(plain("<div>\n<!-- c -->a</div>", 60), ["<div>", "a</div>"]);
}

#[test]
fn table_rows_wrap_to_the_same_width_and_have_aligned_dividers() {
    let lines = plain(
        "| Name | Value |\n| --- | --- |\n| Word | pre aVeryLongTokenWithoutSpaces final |\n| Other | **bold** entry |",
        25,
    );
    assert_eq!(lines[0].chars().next(), Some('┌'));
    assert_eq!(lines.last().and_then(|line| line.chars().next()), Some('└'));
    assert_eq!(lines.iter().filter(|line| line.starts_with('├')).count(), 2);
    let width = visible_width(&lines[0]);
    assert!(
        lines
            .iter()
            .all(|line| visible_width(line) == width && width <= 25),
        "{lines:?}"
    );
}
