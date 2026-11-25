# `pi-rs-schema`

JSON Schema validation (drafts 2020-12 and 7) without `eval`, ported from `@earendil-works/pi-schema`.

`compile()` reads a schema once and builds it into plain checks; `check()` is the fast boolean path, `errors()`
explains a failure keyword by keyword, and `parse()` coerces loosely typed input towards the schema first.

## Usage

```rust
use pi_rs_schema::{Value, compile, compile_coercing, formats, CompileOptions};

let validator = compile(&serde_json::json!({
    "type": "object",
    "properties": { "path": { "type": "string" } },
    "required": ["path"],
    "additionalProperties": false
}))?;

assert!(validator.check(&Value::from(&serde_json::json!({ "path": "a.ts" }))));
assert_eq!(validator.errors(&Value::from(&serde_json::json!({ "path": 1 }))), vec![
    pi_rs_schema::ValidationError::new("/path", "type", "expected string, got integer"),
]);
# Ok::<(), pi_rs_schema::CompileError>(())
```

- `compile(&schema)` — a validator; `compile_with(&schema, &options)` with the draft, the format checks and the
  documents other schemas may reference.
- `compile_coercing(&schema, &options)` — a validator that also coerces; `coerce()` converts a value, `parse()`
  converts and checks it.
- `formats::formats()` — the format checks of JSON Schema 2020-12 (and `url`, `json-pointer-uri-fragment`). Without
  them `format` is an annotation, as draft 2020-12 specifies.
- `builders` — the schema builders: `str`, `num`, `int`, `bool`, `nul`, `bigint`, `bytes`, `unknown`, `lit`,
  `one_of`, `union`, `nullable`, `arr`, `tuple`, `map`, `obj`, `opt`, `recursive`, `unsafe_schema`.
- `equal(&a, &b)` — JSON equality, a drop-in for TypeBox's `Value.Equal`.

## The value model

`Value` is JSON plus the three TypeBox extensions that never pass through JSON: `Value::BigInt`, `Value::Bytes`
and `Value::Undefined`. Numbers are `f64`, as in JavaScript: `-0 == 0`, `NaN != NaN`, and integers beyond 2^53 lose
precision, exactly as the upstream package sees them.

## Verification

- `tests/conformance.rs` runs the vendored official JSON-Schema-Test-Suite of drafts 2020-12 and 7 from
  `tests/fixtures/` (originally from upstream `packages/schema/test/fixtures`): every one of the 4,213 cases passes, and the errors of every case agree with
  its check. The ten cases the upstream package does not pass either are listed in the test.
- `tests/coerce_typebox.rs` replays the 204 cases of TypeBox 1.3.27's `Value.Convert` tests, recorded from the
  upstream TypeScript test file.
- `tests/validator.rs`, `tests/coerce.rs`, `tests/builders.rs`, `tests/equal.rs` and `tests/shared.rs` port the
  upstream unit tests.

```bash
cargo fmt --all -- --check
cargo clippy -p pi-rs-schema --all-targets --locked -- -D warnings
cargo test -p pi-rs-schema --locked
```

## Known differences from the upstream package

- **A bigint constant has no JSON form.** `{"const": 0n}` is a JavaScript value; a schema in Rust writes the
  bigint bounds as decimal strings (`{"type": "bigint", "minimum": "0"}`) and cannot write a bigint `const`.
- **A string holds no unpaired surrogate.** Rust strings are valid UTF-8, so the code point counts of strings with
  unpaired surrogates cannot be reproduced; every other string counts code points as the upstream does.
- **A `bytes` value is not a plain object.** The upstream's coercion treats a `Uint8Array` as a plain object; here a
  `Value::Bytes` is its own kind.
- **A pattern that ECMAScript accepts but no Rust engine compiles** (a lookbehind, for instance) fails to compile
  the schema. The regular expressions themselves are compiled by `regress`, a port of V8's Irregexp, so their
  semantics are exactly JavaScript's.
