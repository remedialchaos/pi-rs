#![forbid(unsafe_code)]
use std::{env, error::Error, fs, path::PathBuf};
fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=assets/prelude-source.ts");
    let source = fs::read_to_string("assets/prelude-source.ts")?;
    let literal = source
        .split_once("export const PRELUDE_SOURCE: string = `")
        .ok_or("missing prelude")?
        .1
        .strip_suffix("`;\n")
        .ok_or("missing template end")?;
    let mut text = String::new();
    let mut chars = literal.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            text.push(match chars.next().ok_or("unterminated escape")? {
                '\\' => '\\',
                '`' => '`',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => return Err(format!("unknown template escape {other}").into()),
            });
        } else {
            text.push(ch);
        }
    }
    for (key, value) in [
        ("${MAX_STORE_VALUE_CHARS}", "262144"),
        ("${MAX_STORE_TOTAL_CHARS}", "1048576"),
        ("${MAX_OUTPUT_CHARS}", "16777216"),
        ("${MAX_OUTPUT_ITEMS}", "100000"),
        (
            "${JSON.stringify(IMAGE_HELPER_EXPECTS)}",
            "\"image expects a non-empty image URL string, an object with image_url, or an image block { type: \\\"image\\\", data, mimeType }\"",
        ),
    ] {
        text = text.replace(key, value);
    }
    if text.contains("${") {
        return Err("unexpanded prelude constant".into());
    }
    fs::write(PathBuf::from(env::var("OUT_DIR")?).join("prelude.js"), text)?;
    Ok(())
}
