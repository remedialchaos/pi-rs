//! `packages/mcp/test/naming.test.ts`.

use pi_rs_mcp::{create_mcp_tool_name, mcp_namespace, mcp_server_summary};

#[test]
fn joins_server_and_tool_and_replaces_characters_outside_the_alphabet() {
    assert_eq!(
        create_mcp_tool_name("docs", "search", |_| false),
        "mcp__docs__search"
    );
    assert_eq!(
        create_mcp_tool_name("my-server", "get.item/v2", |_| false),
        "mcp__my_server__get_item_v2"
    );
}

#[test]
fn sanitizes_per_utf16_code_unit_like_javascript() {
    // JavaScript's `.replace(/[^A-Za-z0-9_]/g, "_")` replaces each UTF-16 code unit, so a
    // surrogate pair contributes two underscores.
    assert_eq!(
        create_mcp_tool_name("docs", "lookup\u{10400}x", |_| false),
        "mcp__docs__lookup__x"
    );
    assert_eq!(
        create_mcp_tool_name("docs", "lookup\u{10400}x", |_| false).len(),
        "mcp__docs__lookup__x".len()
    );
    // A BMP character is one code unit and contributes one underscore.
    assert_eq!(
        create_mcp_tool_name("docs", "lookup\u{00e9}x", |_| false),
        "mcp__docs__lookup_x"
    );
}

#[test]
fn shortens_names_over_64_characters_with_a_hash_of_server_and_tool() {
    let long = create_mcp_tool_name("server", &"x".repeat(100), |_| false);
    assert_eq!(long.chars().count(), 64);
    assert!(long.starts_with("mcp__server__x"));
    // Names that share their first 55 characters still differ.
    assert_ne!(
        create_mcp_tool_name("server", &format!("{}y", "x".repeat(100)), |_| false),
        long
    );
    // The hash covers the unsanitized names, and is stable.
    assert_eq!(
        create_mcp_tool_name("server", &"x".repeat(100), |_| false),
        long
    );
    assert_ne!(
        create_mcp_tool_name("ser-ver", &"x".repeat(100), |_| false),
        create_mcp_tool_name("ser_ver", &"x".repeat(100), |_| false)
    );
}

#[test]
fn adds_the_hash_suffix_when_the_sanitized_name_is_taken() {
    let taken = create_mcp_tool_name("s", "a_b", |_| false);
    assert_eq!(taken, "mcp__s__a_b");
    let second = create_mcp_tool_name("s", "a-b", |name| name == taken);
    assert!(second.starts_with("mcp__s__a_b_"));
    assert!(second.ends_with(&format!("_{}", &second[second.len() - 8..])));
    assert_ne!(
        create_mcp_tool_name("s", "a_b", |name| name == taken),
        second
    );
}

#[test]
fn namespace_is_the_prefix_of_the_servers_tool_names() {
    assert_eq!(mcp_namespace("docs"), "mcp__docs");
    assert_eq!(mcp_namespace("my-server"), "mcp__my_server");
    assert_eq!(mcp_namespace("my-server"), mcp_namespace("my_server"));
    assert!(
        create_mcp_tool_name("my-server", "search", |_| false)
            .starts_with(&format!("{}__", mcp_namespace("my-server")))
    );
}

#[test]
fn summary_takes_the_first_line_of_the_description_else_of_the_instructions() {
    assert_eq!(
        mcp_server_summary(Some(" Docs search.\nMore. "), Some("Ignored.")),
        "Docs search."
    );
    assert_eq!(
        mcp_server_summary(
            None,
            Some("\n  Web search for current information.\nDetails.")
        ),
        "Web search for current information."
    );
    assert_eq!(
        mcp_server_summary(Some("  "), Some("From instructions.")),
        "From instructions."
    );
    assert_eq!(mcp_server_summary(None, None), "");
}
