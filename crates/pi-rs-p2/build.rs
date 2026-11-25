#![forbid(unsafe_code)]

use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

// The WOFF2 names match the vendored fonts emitted by `p2-bundle-assets`.
const ASSETS: &[(&str, &str)] = &[
    ("app.js", "text/javascript; charset=utf-8"),
    ("app_bg.wasm", "application/wasm"),
    ("p2.css", "text/css; charset=utf-8"),
    ("CommitMono-300-Regular.woff2", "font/woff2"),
    ("CommitMono-400-Regular.woff2", "font/woff2"),
    ("CommitMono-700-Regular.woff2", "font/woff2"),
    ("MallosSerif-Italic-Variable.woff2", "font/woff2"),
    ("MallosSerif-Variable.woff2", "font/woff2"),
];

fn main() -> io::Result<()> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=P2_BROWSER_ASSETS");
    // Cargo always provides OUT_DIR to build scripts.
    let out = env::var_os("OUT_DIR").ok_or_else(|| io::Error::other("missing OUT_DIR"))?;
    let out = PathBuf::from(out);
    write_contract_index(&out)?;
    let mut manifest = fs::File::create(out.join("browser_bundle.rs"))?;
    match env::var_os("P2_BROWSER_ASSETS") {
        Some(directory) => embed_bundle(Path::new(&directory), &out, &mut manifest),
        None => writeln!(
            manifest,
            "const BROWSER_BUNDLE: Option<BrowserBundle> = None;"
        ),
    }
}

fn write_contract_index(out: &Path) -> io::Result<()> {
    let directory = "src/api/v1/p2_contracts";
    println!("cargo:rerun-if-changed={directory}");
    let mut declarations = std::collections::BTreeMap::new();
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| io::Error::other("invalid contract filename"))?;
        let declaration: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        declarations.insert(name.to_owned(), declaration["route"] == "session");
    }
    let mut index = fs::File::create(out.join("p2_contract_index.rs"))?;
    writeln!(
        index,
        "fn declaration_source(name: &str) -> Option<&'static str> {{ match name {{"
    )?;
    for name in declarations.keys() {
        let path = format!("/{directory}/{name}.json");
        writeln!(
            index,
            "{name:?} => Some(include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), {path:?}))),"
        )?;
    }
    writeln!(index, "_ => None, }} }}")?;
    writeln!(index, "pub(crate) const SESSION_CONTRACTS: &[&str] = &[")?;
    for (name, _) in declarations.into_iter().filter(|(_, session)| *session) {
        writeln!(index, "{name:?},")?;
    }
    writeln!(index, "];")
}

fn embed_bundle(directory: &Path, out: &Path, manifest: &mut fs::File) -> io::Result<()> {
    let directory = directory.canonicalize()?;
    let css = fs::read_to_string(directory.join("p2.css"))?;
    let out = out.join("browser");
    fs::create_dir_all(&out)?;
    let mut hash = Sha256::new();
    for &(name, mime) in ASSETS {
        let path = directory.join(name);
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes = fs::read(&path).map_err(|error| {
            io::Error::new(error.kind(), format!("{}: {error}", path.display()))
        })?;
        let valid = match mime {
            "application/wasm" => bytes.starts_with(b"\0asm\x01\0\0\0"),
            "font/woff2" => bytes.starts_with(b"wOF2") && css.contains(&format!("./{name}")),
            _ => !bytes.is_empty(),
        };
        if !valid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid browser asset: {}", path.display()),
            ));
        }
        hash.update(name.as_bytes());
        hash.update(Sha256::digest(&bytes));
        fs::write(out.join(name), bytes)?;
    }
    writeln!(
        manifest,
        "const BROWSER_BUNDLE: Option<BrowserBundle> = Some(BrowserBundle {{ base: \"{:x}\", files: &[",
        hash.finalize(),
    )?;
    for &(name, mime) in ASSETS {
        writeln!(
            manifest,
            "({name:?}, {mime:?}, include_bytes!(\"browser/{name}\")),"
        )?;
    }
    writeln!(manifest, "] }});")
}
