use pi_rs_chord::{
    link::decode_text,
    surfaces::{
        NumberFormatOptions, TimeFormatOptions, canonical_locale, create_language, direction_of,
        format_number, format_time, in_language,
    },
};
use serde_json::json;
use std::sync::Arc;

#[test]
fn text_link_rejects_noncanonical_ids_before_normalization()
-> Result<(), Box<dyn std::error::Error>> {
    for uuid in [
        "018cafc0000070008000000000000000",
        "018CAFC0-0000-7000-8000-000000000000",
    ] {
        assert!(decode_text(&serde_json::to_vec(&json!(["h", 1, uuid]))?).is_err());
    }
    decode_text(&serde_json::to_vec(&json!([
        "H",
        1,
        "example",
        1,
        "0123456789abcdef",
        [0, {}],
        ["worker", {}]
    ]))?)?;
    for fingerprint in ["0123456789ABCDEF", "+123456789abcdef"] {
        assert!(
            decode_text(&serde_json::to_vec(&json!([
                "H",
                1,
                "example",
                1,
                fingerprint,
                [0, {}],
                ["worker", {}]
            ]))?)
            .is_err()
        );
    }
    Ok(())
}

#[test]
fn formatting_follows_nested_render_locale_and_likely_script()
-> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(canonical_locale("sh").as_deref(), Some("sr-Latn"));
    assert_eq!(direction_of("az-IR"), "rtl");
    assert_eq!(direction_of("az-Latn-IR"), "ltr");
    let de = create_language("de", Default::default(), |_| {})?;
    let en = create_language("en-US", Arc::clone(&de.translations), |_| {})?;
    in_language(Some(de), || -> pi_rs_chord::Result<()> {
        assert_eq!(format_number(1234.5, None)?, "1.234,5");
        let utc = Some(TimeFormatOptions {
            offset_minutes: Some(0),
        });
        assert_eq!(format_time("2024-02-29T16:05:00+02:00", utc)?, "14:05");
        in_language(Some(en), || -> pi_rs_chord::Result<()> {
            assert!(format_time(50700000_i64, utc)?.starts_with("2:05"));
            assert_eq!(
                format_number(
                    1.005,
                    Some(NumberFormatOptions {
                        minimum_fraction_digits: 2,
                        maximum_fraction_digits: 2,
                        use_grouping: false
                    })
                )?,
                "1.01"
            );
            Ok(())
        })?;
        assert_eq!(format_number(1234.5, None)?, "1.234,5");
        Ok(())
    })?;
    Ok(())
}
