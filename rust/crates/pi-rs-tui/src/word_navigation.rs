//! Word-boundary movement for the line and multiline editors.

use icu_segmenter::WordSegmenter;

use crate::utils::{is_punctuation_char, is_whitespace_char};

#[derive(Debug, Clone, Copy)]
pub struct WordSegment<'a> {
    pub text: &'a str,
    pub word_like: bool,
}

pub type SegmentWords<'a> = dyn for<'s> Fn(&'s str) -> Vec<WordSegment<'s>> + 'a;

#[derive(Default)]
pub struct WordNavigationOptions<'a> {
    pub segment: Option<&'a SegmentWords<'a>>,
    pub is_atomic_segment: Option<&'a dyn Fn(&str) -> bool>,
}

pub(crate) fn segment_words(text: &str) -> impl Iterator<Item = WordSegment<'_>> {
    let mut last = 0;
    WordSegmenter::new_dictionary(Default::default())
        .segment_str(text)
        .iter_with_word_type()
        .skip(1)
        .map(move |(end, kind)| {
            let text = &text[std::mem::replace(&mut last, end)..end];
            // ICU4X can report None for dictionary boundaries adjoining CJK punctuation.
            // Its boundaries are correct, but Intl.Segmenter still classifies the letters as words.
            let word_like = kind.is_word_like() || text.chars().any(char::is_alphanumeric);
            WordSegment { text, word_like }
        })
}

fn segments<'s>(
    text: &'s str,
    custom: Option<&SegmentWords<'_>>,
) -> impl Iterator<Item = WordSegment<'s>> {
    custom
        .map(|segment| segment(text))
        .into_iter()
        .flatten()
        .chain(segment_words(if custom.is_none() { text } else { "" }))
}

fn boundary(text: &str, mut cursor: usize) -> usize {
    cursor = cursor.min(text.len());
    while !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

/// Move backward from a UTF-8 byte offset. Atomic segments are never split.
pub fn find_word_backward(
    text: &str,
    cursor: usize,
    options: Option<&WordNavigationOptions<'_>>,
) -> usize {
    let cursor = boundary(text, cursor);
    let atomic = |segment| {
        options
            .and_then(|opts| opts.is_atomic_segment)
            .is_some_and(|test| test(segment))
    };
    let mut words: Vec<_> =
        segments(&text[..cursor], options.and_then(|opts| opts.segment)).collect();
    let mut end = cursor;
    while let Some(last) = words.pop() {
        let n = last.text.len();
        if atomic(last.text) {
            return end.saturating_sub(n);
        }
        if last.text.chars().any(is_whitespace_char) {
            end = end.saturating_sub(n);
            continue;
        }
        if last.word_like {
            return end.saturating_sub(n)
                + last.text.rfind(is_punctuation_char).map_or(0, |at| at + 1);
        }
        end = end.saturating_sub(n);
        for segment in words.into_iter().rev() {
            if atomic(segment.text)
                || segment.word_like
                || segment.text.chars().any(is_whitespace_char)
            {
                break;
            }
            end = end.saturating_sub(segment.text.len());
        }
        break;
    }
    end
}

/// Move forward from a UTF-8 byte offset. Atomic segments are never split.
pub fn find_word_forward(
    text: &str,
    cursor: usize,
    options: Option<&WordNavigationOptions<'_>>,
) -> usize {
    let mut cursor = boundary(text, cursor);
    let atomic = |segment| {
        options
            .and_then(|opts| opts.is_atomic_segment)
            .is_some_and(|test| test(segment))
    };
    let mut words = segments(&text[cursor..], options.and_then(|opts| opts.segment));
    while let Some(first) = words.next() {
        let n = first.text.len();
        if atomic(first.text) {
            return cursor + n;
        }
        if first.text.chars().any(is_whitespace_char) {
            cursor += n;
            continue;
        }
        if first.word_like {
            return cursor + first.text.find(is_punctuation_char).unwrap_or(n);
        }
        cursor += n;
        for segment in words {
            if atomic(segment.text)
                || segment.word_like
                || segment.text.chars().any(is_whitespace_char)
            {
                break;
            }
            cursor += segment.text.len();
        }
        break;
    }
    cursor
}
