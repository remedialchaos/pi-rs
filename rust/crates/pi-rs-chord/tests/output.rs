use pi_rs_chord::{Result, json::JsonString, output::*};

fn limits(retain: Retain, max_bytes: usize, max_lines: usize) -> OutputLimits {
    OutputLimits {
        retain,
        max_bytes,
        max_lines,
    }
}
#[test]
fn output_stream_flushes_utf8_boundaries_strips_only_initial_byte_bom_and_counts_unsanitized_text()
-> Result<()> {
    let mut buffer = OutputBuffer::new(limits(Retain::Tail, 1000, 2));
    buffer.push_bytes(&[0xef], None)?;
    buffer.push_bytes(&[0xbb, 0xbf, b'a', b'\n', b'b', b'\n', 0xf0, 0x9f], None)?;
    buffer.push_bytes(&[0x98, 0x80, b'\n'], None)?;
    let slice = buffer.snapshot();
    assert_eq!(slice.text, JsonString::from("b\n😀\n"));
    assert_eq!((slice.dropped_bytes, slice.dropped_lines), (2, 1));
    buffer.push_bytes(&[0xe2], None)?;
    buffer.push_text(&JsonString::from("x"), None)?;
    buffer.push_bytes(&[0xef, 0xbb, 0xbf], None)?;
    buffer.end();
    assert_eq!(buffer.snapshot().text, JsonString::from("😀\n�x\u{feff}"));
    let mut ansi = OutputBuffer::new(limits(Retain::Tail, 1024, 100));
    for s in ["\u{1b}[3", "1mERROR\u{1b}[0", "m\n"] {
        ansi.push_text(&s.into(), None)?;
    }
    assert_eq!(ansi.snapshot().text, JsonString::from("ERROR\n"));
    assert_eq!(
        sanitize_output(&"a\0b\tc\nd\re\u{7}f\u{fff9}g\u{fffb}h😀".into()),
        JsonString::from("ab\tc\ndefgh😀")
    );
    let lone = JsonString(vec![b'a' as u16, 0xd800]);
    assert_eq!(bound_output(&lone, limits(Retain::Tail, 100, 1)).text, lone);
    assert_eq!(
        bound_output(&lone, limits(Retain::Tail, 3, 1)).text,
        JsonString::from("�")
    );
    Ok(())
}
#[test]
fn output_skip_totals_and_snapshot_compaction_match_uninterrupted_streams() -> Result<()> {
    let l = limits(Retain::Tail, 1000, 2);
    let mut all = OutputBuffer::new(l);
    let mut skipping = OutputBuffer::new(l);
    all.push_text(&"one\ntwo\nthree\nfour\nfive\nsix".into(), None)?;
    skipping.push_text(
        &"ee\nfour\nfive\nsix".into(),
        Some(OutputSkip {
            bytes: 11,
            newlines: 2,
            ends_with_newline: false,
        }),
    )?;
    assert_eq!(all.snapshot(), skipping.snapshot());
    let snap = skipping.snapshot();
    assert_eq!(snap.text, JsonString::from("five\nsix"));
    assert_eq!((snap.dropped_bytes, snap.dropped_lines), (19, 4));
    assert!(
        OutputBuffer::new(limits(Retain::Head, 10, 2))
            .push_text(
                &"x".into(),
                Some(OutputSkip {
                    bytes: 3,
                    newlines: 1,
                    ends_with_newline: true
                })
            )
            .is_err()
    );
    let mut seed = 0x12345678u32;
    for n in 0..500 {
        let l = limits(
            if n % 2 == 0 {
                Retain::Head
            } else {
                Retain::Tail
            },
            n % 40 + 1,
            n % 5 + 1,
        );
        let mut sampled = OutputBuffer::new(l);
        let mut full = Vec::new();
        for _ in 0..30 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let chunk = ["é\n", "line\n", "😀", "\n", "a", "\t", "\u{1b}"][(seed as usize) % 7];
            sampled.push_text(&chunk.into(), None)?;
            full.extend(chunk.encode_utf16());
            if seed.is_multiple_of(3) {
                sampled.snapshot();
            }
        }
        let expected = bound_output(&JsonString(full), l);
        let actual = sampled.snapshot();
        assert_eq!(actual.text, sanitize_output(&expected.text));
        assert_eq!(
            (actual.dropped_bytes, actual.dropped_lines),
            (expected.dropped_bytes, expected.dropped_lines)
        );
        assert!(sampled.stored_bytes() <= l.max_bytes + 12);
    }
    Ok(())
}
