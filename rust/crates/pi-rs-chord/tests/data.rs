use pi_rs_chord::{
    Result,
    data::{ChordError, Validator, join_key, principal_json, principal_key},
    json::{JsonString, JsonValue, number_to_string},
};
use serde_json::json;

#[test]
fn json_retains_surrogates_own_keys_and_javascript_number_spelling() -> Result<()> {
    let input = r#"{"text":"😀","\ud800":"\udc00","__proto__":1}"#;
    let value = JsonValue::parse(input)?;
    assert_eq!(value.to_json()?, input);
    assert_eq!(serde_json::to_string(&value)?, input);
    assert_eq!(JsonValue::parse(&value.to_json()?)?, value);
    assert_eq!(JsonString(vec![0xdc00]).to_json(), r#""\udc00""#);
    assert!(JsonString(vec![0xdc00]).to_utf8().is_err());
    assert_eq!(number_to_string(-0.0), "0");
    assert_eq!(number_to_string(1e21), "1e+21");
    assert_eq!(number_to_string(1e-7), "1e-7");
    assert_eq!(number_to_string(1e-6), "0.000001");
    assert!(!pi_rs_chord::json::is_json_value(&JsonValue::Number(
        f64::NAN
    )));
    assert_eq!(
        JsonValue::parse(r#"{"10":1,"2":2,"a":3}"#)?.to_json()?,
        r#"{"2":2,"10":1,"a":3}"#
    );
    Ok(())
}

#[test]
fn principal_identity_sorts_keys_and_validation_preserves_error_pointer() -> Result<()> {
    let a = json!({"kind":"user","nested":{"10":1,"2":2}, "n":-0.0});
    let b = json!({"n":0,"nested":{"2":2,"10":1},"kind":"user"});
    assert_eq!(principal_key(&a)?, principal_key(&b)?);
    assert!(principal_key(&a)?.contains(r#""10":1,"2":2"#));
    assert_ne!(principal_json(&a)?, principal_json(&b)?);
    assert_eq!(join_key(&["x|y".into(), "z\\w".into()]), "x\\|y|z\\\\w");
    assert!(ChordError::from_json(json!({"code":"invalid","message":"x"})).is_err());
    assert!(ChordError::from_json(json!({"code":"gone","message":"x","extra":1})).is_err());
    assert!(
        ChordError::from_json(json!({"code":"unavailable","reason":"busy","message":"x"})).is_ok()
    );
    let validator = Validator::compile(
        &json!({"type":"object", "properties":{"n":{"type":"integer"}},"required":["n"],"additionalProperties":false}),
    )?;
    let error = validator.check(&json!({}), "args");
    assert!(
        matches!(error, Err(pi_rs_chord::Error::Chord(ref e)) if e.code=="invalid" && e.details==Some(json!({"path":"/n"})))
    );
    Ok(())
}
