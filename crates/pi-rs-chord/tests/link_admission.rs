use pi_rs_chord::{
    Result,
    json::JsonValue,
    link::{self, Body, Message, Part},
};
#[test]
fn wire_errors_are_closed_tagged_envelopes_and_chord_details_stay_opaque() -> Result<()> {
    for text in [
        "null",
        r#"{"code":"x","message":"bad"}"#,
        r#"{"kind":"plugin","code":"x","message":1}"#,
        r#"{"kind":"chord","error":{},"extra":true}"#,
    ] {
        assert!(
            Message::new(Body::Error {
                call: 0,
                error: JsonValue::parse(text)?
            })
            .validate()
            .is_err(),
            "{text}"
        );
    }
    Message::new(Body::Error {
        call: 0,
        error: JsonValue::parse(r#"{"kind":"chord","error":{"private":true}}"#)?,
    })
    .validate()?;
    assert!(
        Message::new(Body::Abort {
            call: link::MAX_SAFE_INTEGER + 1
        })
        .validate()
        .is_err()
    );
    Ok(())
}
#[test]
fn hold_principal_requires_string_kind_without_parsing_opaque_parts() -> Result<()> {
    let mut message = Message::new(Body::Hold {
        hold: 0,
        type_name: "t".into(),
        version: 1,
        fingerprint: [0; 8],
        params: Part::Bytes(vec![255]),
        address_kind: "h".into(),
        address_params: Box::new(JsonValue::Null),
        principal: Some(Box::new(JsonValue::parse(r#"{"kind":7}"#)?)),
        incarnation: None,
        chain: None,
        limit: None,
    });
    assert!(message.validate().is_err());
    if let Body::Hold { principal, .. } = &mut message.body {
        *principal = Some(Box::new(JsonValue::parse(
            r#"{"kind":"guest","custom":{"x":1}}"#,
        )?));
    }
    message.validate()?;
    assert_eq!(link::decode(&link::encode(&message)?)?, message);
    Ok(())
}
