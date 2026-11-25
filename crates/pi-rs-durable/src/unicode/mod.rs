//! Unicode 17 normalization, using the `unicode-normalization` 0.1.25 generated
//! tables (license alongside the tables). No additional Cargo dependency.

mod lookups;
mod perfect_hash;
#[rustfmt::skip]
mod tables;
pub(crate) mod words;

use std::borrow::Cow;

use lookups::{
    canonical_combining_class, canonical_fully_decomposed, compatibility_fully_decomposed,
    composition_table,
};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum QuickCheck {
    Yes,
    No,
    Maybe,
}

pub(crate) fn nfc(text: &str) -> Cow<'_, str> {
    normalize(text, tables::qc_nfc, false, true)
}
pub(crate) fn nfd(text: &str) -> Cow<'_, str> {
    normalize(text, tables::qc_nfd, false, false)
}
pub(crate) fn nfkc(text: &str) -> Cow<'_, str> {
    normalize(text, tables::qc_nfkc, true, true)
}

const S_BASE: u32 = 0xac00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11a7;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;
const S_COUNT: u32 = 19 * N_COUNT;

fn compose(a: char, b: char) -> Option<char> {
    let (c, next) = (u32::from(a), u32::from(b));
    if (L_BASE..L_BASE + 19).contains(&c) && (V_BASE..V_BASE + V_COUNT).contains(&next) {
        return char::from_u32(S_BASE + (c - L_BASE) * N_COUNT + (next - V_BASE) * T_COUNT);
    }
    if (S_BASE..S_BASE + S_COUNT).contains(&c)
        && (c - S_BASE).is_multiple_of(T_COUNT)
        && (T_BASE + 1..T_BASE + T_COUNT).contains(&next)
    {
        return char::from_u32(c + next - T_BASE);
    }
    composition_table(a, b)
}

fn decompose(c: char, compatible: bool, mut emit: impl FnMut(char)) {
    let cp = u32::from(c);
    if cp < 0x80 {
        emit(c);
        return;
    }
    if (S_BASE..S_BASE + S_COUNT).contains(&cp) {
        let syllable = cp - S_BASE;
        let tail = syllable % T_COUNT;
        for jamo in [
            Some(L_BASE + syllable / N_COUNT),
            Some(V_BASE + (syllable % N_COUNT) / T_COUNT),
            (tail != 0).then_some(T_BASE + tail),
        ]
        .into_iter()
        .flatten()
        .filter_map(char::from_u32)
        {
            emit(jamo);
        }
        return;
    }
    let expanded = if compatible {
        compatibility_fully_decomposed(c)
    } else {
        None
    }
    .or_else(|| canonical_fully_decomposed(c));
    if let Some(expanded) = expanded {
        expanded.iter().copied().for_each(emit);
    } else {
        emit(c);
    }
}

fn normalize(
    text: &str,
    quick_check: fn(char) -> QuickCheck,
    compatible: bool,
    composed: bool,
) -> Cow<'_, str> {
    let mut prev = 0;
    if text.is_ascii()
        || text.chars().all(|c| {
            let class = canonical_combining_class(c);
            let ordered = class == 0 || prev <= class;
            prev = class;
            ordered && quick_check(c) == QuickCheck::Yes
        })
    {
        return Cow::Borrowed(text);
    }

    let mut chars = Vec::new();
    let mut first_mark = 0;
    for c in text.chars() {
        decompose(c, compatible, |d| {
            let class = canonical_combining_class(d);
            if class == 0 {
                chars[first_mark..].sort_by_key(|pair: &(char, u8)| pair.1);
                first_mark = chars.len() + 1;
            }
            chars.push((d, class));
        });
    }
    chars[first_mark..].sort_by_key(|pair| pair.1);
    if composed {
        let mut len = 0;
        let mut base: Option<usize> = None;
        let mut last_class = 0;
        for read in 0..chars.len() {
            let (c, class) = chars[read];
            if let Some(start) = base
                && (last_class == 0 || last_class < class)
                && let Some(next) = compose(chars[start].0, c)
            {
                chars[start].0 = next;
                continue;
            }
            if class == 0 {
                base = Some(len);
            }
            chars[len] = (c, class);
            len += 1;
            last_class = class;
        }
        chars.truncate(len);
    }
    let mut normalized = String::with_capacity(text.len());
    normalized.extend(chars.into_iter().map(|(c, _)| c));
    if normalized == text {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(normalized)
    }
}
