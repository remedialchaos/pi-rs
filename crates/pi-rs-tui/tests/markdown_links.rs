use pi_rs_tui::{
    Component, DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme, TerminalCapabilities,
    set_capabilities, strip_terminal_sequences,
};

fn render(source: &str) -> String {
    Markdown::new(
        source,
        0,
        0,
        MarkdownTheme::default(),
        DefaultTextStyle::default(),
        MarkdownOptions::default(),
    )
    .render(160)
    .join("")
}

#[test]
fn bare_links_stop_at_cjk_punctuation_but_explicit_destinations_do_not() {
    set_capabilities(TerminalCapabilities {
        hyperlinks: true,
        ..Default::default()
    });
    for punctuation in
        "。，；：！？、\u{201c}\u{201d}‘’（）【】《》「」『』…—·・〈〉［］｛｝．｡｢｣､･".chars()
    {
        for url in [
            "https://example.com",
            "https://example.com/中文?q=搜索#片段",
            "www.example.com",
        ] {
            let source = format!("{url}{punctuation}后面的中文");
            let output = render(&source);
            let http = if url.starts_with("www.") {
                "http://"
            } else {
                ""
            };
            assert!(
                output.contains(&format!("\x1b]8;;{http}{url}\x1b\\")),
                "{source}: {output:?}"
            );
            assert!(
                output.contains(&format!("\x1b]8;;\x1b\\{punctuation}后面的中文")),
                "{source}: {output:?}"
            );
            assert_eq!(strip_terminal_sequences(&output).trim_end(), source);
        }
    }
    let explicit = "https://example.com/中文，路径·テスト・한글…—。";
    for source in [format!("[文档]({explicit})"), format!("<{explicit}>")] {
        assert!(render(&source).contains(&format!("\x1b]8;;{explicit}\x1b\\")));
    }
    set_capabilities(TerminalCapabilities::default());
    assert_eq!(
        strip_terminal_sequences(&render("[text](https://example.com)")).trim_end(),
        "text (https://example.com)"
    );
    assert_eq!(
        strip_terminal_sequences(&render("Contact user@example.com for help")).trim_end(),
        "Contact user@example.com for help"
    );
}
