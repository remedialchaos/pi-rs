use indexmap::IndexMap;

pub(super) fn launch(
    command: String,
    args: Vec<String>,
) -> (Vec<String>, IndexMap<String, String>) {
    let lower = command.to_ascii_lowercase().replace('/', "\\");
    if lower.ends_with(".exe") || lower.ends_with(".com") {
        return (
            std::iter::once(command).chain(args).collect(),
            IndexMap::new(),
        );
    }
    let shim = lower.rsplit_once('\\').is_some_and(|(directory, file)| {
        (directory == "node_modules\\.bin" || directory.ends_with("\\node_modules\\.bin"))
            && file.ends_with(".cmd")
    });
    let mut line = escape(&command.replace('/', "\\"));
    for argument in args {
        line.push(' ');
        let escaped = escape(&quote(&argument));
        if shim {
            line.push_str(&escape(&escaped));
        } else {
            line.push_str(&escaped);
        }
    }
    (
        ["cmd.exe", "/d", "/s", "/c", "%P2_MCP_COMMAND%"]
            .map(str::to_owned)
            .into(),
        IndexMap::from([("P2_MCP_COMMAND".into(), line)]),
    )
}

fn escape(value: &str) -> String {
    let mut output = String::new();
    for ch in value.chars() {
        if r#"()[]%!^"`<>&|;, *?"#.contains(ch) {
            output.push('^');
        }
        output.push(ch);
    }
    output
}

fn quote(argument: &str) -> String {
    let mut output = String::from("\"");
    let mut slashes = 0;
    for character in argument.chars() {
        if character == '\\' {
            slashes += 1;
            continue;
        }
        let count = if character == '"' {
            2 * slashes + 1
        } else {
            slashes
        };
        output.extend(std::iter::repeat_n('\\', count));
        output.push(character);
        slashes = 0;
    }
    output.extend(std::iter::repeat_n('\\', 2 * slashes));
    output.push('"');
    output
}
