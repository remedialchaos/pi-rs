// The official JSON-Schema-Test-Suite, drafts 2020-12 and 7: every test, including optional and format tests.
// A test listed in KNOWN_FAILURES must fail; when one starts passing, remove it from the list.

use indexmap::IndexMap;
use pi_rs_schema::{CompileOptions, Draft, JsonSchema, UnresolvedRefs, Value, compile_with};
use std::path::{Path, PathBuf};

/// The tests the upstream package does not pass either.
const KNOWN_FAILURES: &[&str] = &[
    // Vocabularies switched off by a custom meta-schema are not supported.
    "draft2020-12/vocabulary > schema that uses custom metaschema with with no validation vocabulary > no validation: invalid number, but it still validates",
    "draft2020-12/optional/format-assertion > schema that uses custom metaschema with format-assertion: false > format-assertion: false: invalid string",
    "draft2020-12/optional/format-assertion > schema that uses custom metaschema with format-assertion: true > format-assertion: true: invalid string",
    // A $ref to a document of another draft is evaluated with the compile's draft.
    "draft2020-12/optional/cross-draft > refs to historic drafts are processed as historic drafts > first item not a string is valid",
    "draft7/optional/cross-draft > refs to future drafts are processed as future drafts > missing bar is invalid",
    // contentMediaType and contentEncoding are annotations.
    "draft7/optional/content > validation of string-encoded content based on media type > an invalid JSON document",
    "draft7/optional/content > validation of binary string-encoding > an invalid base64 string (% is not a valid character)",
    "draft7/optional/content > validation of binary-encoded media type documents > a validly-encoded invalid JSON document",
    "draft7/optional/content > validation of binary-encoded media type documents > an invalid base64 string that is valid JSON",
];

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Every JSON file of a directory, by path.
fn json_files(directory: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return files;
    };
    let mut entries: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            files.extend(json_files(&path));
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
            files.push(path);
        }
    }
    files
}

/// The remote documents and meta-schemas the tests reference, by URI.
fn suite_schemas() -> IndexMap<String, JsonSchema> {
    let mut schemas = IndexMap::new();
    let root = fixtures().join("json-schema-test-suite");
    let remotes = root.join("remotes");
    for path in json_files(&remotes) {
        let name = path
            .strip_prefix(&remotes)
            .expect("the file is below the remotes")
            .to_string_lossy()
            .replace('\\', "/");
        schemas.insert(format!("http://localhost:1234/{name}"), read_json(&path));
    }
    let meta = fixtures().join("meta-schemas");
    schemas.insert(
        "http://json-schema.org/draft-07/schema".to_string(),
        read_json(&meta.join("draft-07-schema.json")),
    );
    schemas.insert(
        "https://json-schema.org/draft/2020-12/schema".to_string(),
        read_json(&meta.join("draft2020-12-schema.json")),
    );
    for vocabulary in [
        "core",
        "applicator",
        "unevaluated",
        "validation",
        "meta-data",
        "format-annotation",
        "content",
    ] {
        schemas.insert(
            format!("https://json-schema.org/draft/2020-12/meta/{vocabulary}"),
            read_json(&meta.join(format!("draft2020-12-meta-{vocabulary}.json"))),
        );
    }
    schemas
}

/// The groups of a test file.
fn read_groups(path: &Path) -> Vec<serde_json::Value> {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn read_json(path: &Path) -> JsonSchema {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

#[test]
fn the_official_test_suite_passes() {
    let root = fixtures().join("json-schema-test-suite");
    let schemas = suite_schemas();
    let mut passed = 0usize;
    let mut total = 0usize;
    let mut failed: Vec<String> = Vec::new();
    let mut unexpected: Vec<String> = Vec::new();

    for (directory, draft) in [("draft2020-12", Draft::V2020_12), ("draft7", Draft::V7)] {
        let tests = root.join("tests").join(directory);
        for path in json_files(&tests) {
            let name = path
                .strip_prefix(root.join("tests"))
                .expect("the file is below the tests")
                .to_string_lossy()
                .replace('\\', "/")
                .trim_end_matches(".json")
                .to_string();
            let groups = read_groups(&path);
            for group in &groups {
                let description = group
                    .get("description")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let schema = group
                    .get("schema")
                    .cloned()
                    .unwrap_or(JsonSchema::Bool(true));
                let mut options = CompileOptions::new()
                    .with_draft(draft)
                    .with_schemas(schemas.clone());
                if name.contains("/optional/format/") {
                    options = options.with_formats(pi_rs_schema::formats::formats());
                }
                let tests = group
                    .get("tests")
                    .and_then(|value| value.as_array())
                    .cloned()
                    .unwrap_or_default();
                for test in tests {
                    let test_description = test
                        .get("description")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    let data = Value::from(test.get("data").unwrap_or(&JsonSchema::Null));
                    let valid = test
                        .get("valid")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false);
                    total += 1;
                    let key = format!("{name} > {description} > {test_description}");
                    let known = KNOWN_FAILURES.contains(&key.as_str());
                    let outcome = match compile_with(&schema, &options) {
                        Ok(validator) => {
                            let checked = validator.check(&data);
                            let errors = validator.errors(&data);
                            let agrees = errors.is_empty() == checked;
                            Some((checked == valid, agrees))
                        }
                        Err(error) => {
                            if known {
                                Some((false, true))
                            } else {
                                unexpected.push(format!("{key}: {error}"));
                                None
                            }
                        }
                    };
                    match outcome {
                        Some((true, true)) => passed += 1,
                        Some((false, true)) => {
                            if known {
                                passed += 1;
                            } else {
                                failed.push(key);
                            }
                        }
                        Some((_, false)) => {
                            failed.push(format!("{key} (errors disagree with check)"));
                        }
                        None => {}
                    }
                }
            }
        }
    }

    if !unexpected.is_empty() {
        panic!(
            "{} cases did not compile:\n{}",
            unexpected.len(),
            unexpected.join("\n")
        );
    }
    if !failed.is_empty() {
        panic!("{} cases failed:\n{}", failed.len(), failed.join("\n"));
    }
    assert_eq!(passed, total, "not every case ran");
    assert!(total > 4000, "only {total} cases ran");
    println!("{passed} cases passed");
}

#[test]
fn the_unresolved_references_option_rejects_without_throwing() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": { "t": { "$ref": "#/definitions/Missing" } }
    });
    let options = CompileOptions::new().with_unresolved_refs(UnresolvedRefs::Reject);
    let validator = compile_with(&schema, &options).expect("the schema compiles");
    assert!(validator.check(&Value::from(&serde_json::json!({}))));
    assert!(!validator.check(&Value::from(&serde_json::json!({ "t": 1 }))));
    assert_eq!(
        validator.errors(&Value::from(&serde_json::json!({ "t": 1 }))),
        vec![pi_rs_schema::ValidationError::new(
            "/t",
            "$ref",
            "unresolved reference #/definitions/Missing"
        )]
    );
}
