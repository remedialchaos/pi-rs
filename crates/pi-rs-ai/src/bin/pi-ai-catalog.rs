//! Generates the model catalog assets the crate embeds, from a hydrated model-data
//! directory, like upstream's `packages/ai/catalog/generate.ts`.
//!
//! Usage: `pi-ai-catalog --hydrated <dir> --out <dir> [--providers <file>] [--check]`

use std::path::PathBuf;
use std::process::ExitCode;

use pi_rs_ai::catalog_generate;

fn usage() -> &'static str {
    "Usage: pi-ai-catalog --hydrated <dir> --out <dir> [--providers <file>] [--check]"
}

struct Options {
    hydrated: PathBuf,
    out: PathBuf,
    providers: Option<PathBuf>,
    check: bool,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut hydrated = None;
    let mut out = None;
    let mut providers = None;
    let mut check = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--hydrated" => {
                index += 1;
                hydrated = Some(PathBuf::from(value(args, index)?));
            }
            "--out" => {
                index += 1;
                out = Some(PathBuf::from(value(args, index)?));
            }
            "--providers" => {
                index += 1;
                providers = Some(PathBuf::from(value(args, index)?));
            }
            "--check" => check = true,
            other => return Err(format!("Unknown argument: {other}")),
        }
        index += 1;
    }
    Ok(Options {
        hydrated: hydrated.ok_or("--hydrated requires a directory")?,
        out: out.ok_or("--out requires a directory")?,
        providers,
        check,
    })
}

fn value(args: &[String], index: usize) -> Result<String, String> {
    args.get(index)
        .cloned()
        .ok_or_else(|| "missing value".to_owned())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}\n{}", usage());
            return ExitCode::FAILURE;
        }
    };
    let catalog = match catalog_generate::generate(&options.hydrated) {
        Ok(catalog) => catalog,
        Err(error) => {
            eprintln!("catalog generation failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let summary = catalog_generate::summarize(&catalog);
    if options.check {
        if let Err(error) = catalog_generate::check(&catalog, &options.out) {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    } else if let Err(error) = catalog_generate::write(&catalog, &options.out) {
        eprintln!("catalog generation failed: {error}");
        return ExitCode::FAILURE;
    }
    if let Some(providers) = &options.providers {
        let destination = options.out.join(catalog_generate::PROVIDERS_FILE);
        let result = if options.check {
            std::fs::read(providers).and_then(|expected| {
                if std::fs::read(&destination)? == expected {
                    Ok(())
                } else {
                    Err(std::io::Error::other("provider catalog is out of date"))
                }
            })
        } else {
            std::fs::copy(providers, destination).map(|_| ())
        };
        if let Err(error) = result {
            eprintln!("provider catalog {}: {error}", providers.display());
            return ExitCode::FAILURE;
        }
    }
    println!("Model statistics:");
    println!("  providers: {}", summary.providers);
    println!("  chat models: {}", summary.chat_models);
    println!("  reasoning models: {}", summary.reasoning_models);
    println!("  image models: {}", summary.image_models);
    println!("  classifier models: {}", summary.classifier_models);
    ExitCode::SUCCESS
}
