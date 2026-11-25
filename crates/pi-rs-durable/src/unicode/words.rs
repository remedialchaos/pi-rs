//! Word breaks after the search tokenizer removes punctuation. Dictionary
//! costs and Southeast Asian lookahead follow ICU 78's `dictbe.cpp`.

use std::sync::LazyLock;

use regex::Regex;

use super::{canonical_combining_class, nfkc};

static RUNS: LazyLock<Regex> = LazyLock::new(|| {
    // INFALLIBLE: This literal uses only supported Unicode script properties.
    Regex::new(concat!(
        r"(?P<cj>[\p{Han}\p{Hiragana}\p{Katakana}\u{30fc}\u{ff70}\u{ff9e}\u{ff9f}\u{3099}\u{309a}]+)",
        r"|(?P<hangul>[\u{ac00}-\u{d7a3}]+)",
        r"|(?P<thai>[\p{Thai}&&[\p{L}\p{M}]]+)",
        r"|(?P<lao>[\p{Lao}&&[\p{L}\p{M}]]+)",
        r"|(?P<khmer>[\p{Khmer}&&[\p{L}\p{M}]]+)",
        r"|(?P<myanmar>[\p{Myanmar}&&[\p{L}\p{M}]]+)",
    )).expect("static dictionary script pattern")
});

static MARK: LazyLock<Regex> = LazyLock::new(|| {
    // INFALLIBLE: This literal is one Unicode general category.
    Regex::new(r"^\p{M}").expect("static combining mark pattern")
});

pub(crate) fn for_each_word<'a>(word: &'a str, mut emit: impl FnMut(&'a str)) {
    if word.is_ascii() {
        emit(word);
        return;
    }
    let mut start = 0;
    for run in RUNS.captures_iter(word) {
        let Some(text) = run.get(0) else {
            continue;
        };
        let script = match () {
            () if run.name("thai").is_some() => Some(Regional::Thai),
            () if run.name("lao").is_some() => Some(Regional::Lao),
            () if run.name("khmer").is_some() => Some(Regional::Khmer),
            () if run.name("myanmar").is_some() => Some(Regional::Myanmar),
            () => None,
        };
        if let Some(script) = script {
            // Only interior dictionary breaks split a Complex_Context run
            // from the adjacent letters or numbers in an ICU word.
            let mut end = text.start();
            regional_breaks(text.as_str(), script, &mut |segment| {
                end += segment.len();
                if end < text.end() {
                    emit(&word[start..end]);
                    start = end;
                }
            });
        } else {
            let plain = &word[start..text.start()];
            if plain.chars().any(char::is_alphanumeric) {
                emit(plain);
            }
            if run.name("hangul").is_some() {
                emit(text.as_str());
            } else {
                cjk_breaks(text.as_str(), &mut emit);
            }
            start = text.end();
        }
    }
    let plain = &word[start..];
    if plain.chars().any(char::is_alphanumeric) {
        emit(plain);
    }
}

#[derive(Clone, Copy)]
struct Dictionary {
    data: &'static str,
    start: usize,
}

impl Dictionary {
    const fn new(data: &'static str) -> Self {
        let mut start = 0;
        while start + 1 < data.len() {
            if data.as_bytes()[start] == b'\n' && data.as_bytes()[start + 1] == b'\n' {
                start += 2;
                break;
            }
            start += 1;
        }
        Self { data, start }
    }

    /// Bytewise lower bound over sorted LF records; midpoint scans stay inside one line.
    fn lower_bound(self, prefix: &str) -> Option<(&'static str, Option<&'static str>)> {
        let mut lo = self.start;
        let mut hi = self.data.len();
        let bytes = self.data.as_bytes();
        while lo < hi {
            let mid = lo.midpoint(hi);
            let start = bytes[lo..mid]
                .iter()
                .rposition(|&byte| byte == b'\n')
                .map_or(lo, |i| lo + i + 1);
            let end = bytes[mid..hi]
                .iter()
                .position(|&byte| byte == b'\n')
                .map_or(hi, |i| mid + i);
            let row = &self.data[start..end];
            let key = row.split_once('\t').map_or(row, |(key, _)| key);
            if key < prefix {
                lo = (end + 1).min(hi);
            } else {
                hi = start;
            }
        }
        let row = self.data[lo..].lines().next()?;
        let (word, cost) = row
            .split_once('\t')
            .map_or((row, None), |(word, cost)| (word, Some(cost)));
        word.starts_with(prefix).then_some((word, cost))
    }

    /// Search complete word candidates and report the number of characters matching a prefix.
    fn candidates(
        self,
        text: &str,
        start: usize,
        offsets: &[usize],
        mut visit: impl FnMut(usize, Option<u32>),
    ) -> usize {
        let mut prefix_len = 0;
        for (index, &end) in offsets.iter().enumerate().skip(start + 1).take(20) {
            let candidate = &text[offsets[start]..end];
            let Some((word, cost)) = self.lower_bound(candidate) else {
                break;
            };
            prefix_len += 1;
            if word == candidate {
                visit(index, cost.and_then(|cost| cost.parse().ok()));
            }
        }
        prefix_len
    }
}

const CJK: Dictionary = Dictionary::new(include_str!("data/cjdict.txt"));

fn cjk_breaks<'a>(text: &'a str, emit: &mut impl FnMut(&'a str)) {
    // NFKC changes candidate lookup, not the spelling returned to the caller.
    // Positions inside a compatibility expansion map to the beginning of the source character.
    let mut normalized = nfkc(text);
    let mut original = Vec::new();
    if matches!(normalized, std::borrow::Cow::Owned(_)) {
        let mut mapped = String::with_capacity(normalized.len());
        let mut starts = text
            .char_indices()
            .filter_map(|(index, c)| {
                (index == 0
                    || (canonical_combining_class(c) == 0 && !matches!(c, '\u{ff9e}' | '\u{ff9f}')))
                .then_some(index)
            })
            .chain(std::iter::once(text.len()));
        if let Some(mut start) = starts.next() {
            for end in starts {
                let part = nfkc(&text[start..end]);
                original.extend(std::iter::repeat_n(start, part.chars().count()));
                mapped.push_str(&part);
                start = end;
            }
        }
        original.push(text.len());
        normalized = std::borrow::Cow::Owned(mapped);
    }
    let positions: Vec<_> = normalized
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(normalized.len()))
        .collect();
    let char_count = positions.len() - 1;
    let mut best = vec![(u64::MAX, 0); positions.len()];
    best[0].0 = 0;
    let mut previous_katakana = false;
    for index in 0..char_count {
        let base = best[index].0;
        let mut consider = |end, cost: u32| {
            let score = base + u64::from(cost);
            let target: &mut (u64, usize) = &mut best[end];
            if score < target.0 {
                *target = (score, index);
            }
        };
        let mut single = false;
        CJK.candidates(&normalized, index, &positions, |end, cost| {
            if let Some(cost) = cost {
                single |= end == index + 1;
                consider(end, cost);
            }
        });
        if !single {
            consider(index + 1, 255);
        }
        let katakana = normalized[positions[index]..].starts_with(is_katakana);
        if katakana && !previous_katakana {
            const COST: [u32; 9] = [8192, 984, 408, 240, 204, 252, 300, 372, 480];
            let run = normalized[positions[index]..]
                .chars()
                .take(20)
                .take_while(|&c| is_katakana(c))
                .count();
            if run < 20 {
                consider(index + run, COST.get(run).copied().unwrap_or(8192));
            }
        }
        previous_katakana = katakana;
    }
    let position = |index| {
        if original.is_empty() {
            positions[index]
        } else {
            original[index]
        }
    };
    let mut ends = Vec::new();
    let mut end = char_count;
    while end > 0 {
        let byte = position(end);
        if ends.last() != Some(&byte) {
            ends.push(byte);
        }
        end = best[end].1;
    }
    let mut start = 0;
    for end in ends.into_iter().rev() {
        if end > start
            && let Some(segment) = text.get(start..end)
        {
            emit(segment);
        }
        start = end;
    }
}

fn is_katakana(c: char) -> bool {
    matches!(c, '\u{30a1}'..='\u{30fa}' | '\u{30fc}'..='\u{30fe}' | '\u{ff66}'..='\u{ff9f}')
}

#[derive(Clone, Copy)]
enum Regional {
    Thai,
    Lao,
    Khmer,
    Myanmar,
}

impl Regional {
    fn dictionary(self) -> Dictionary {
        const THAI: Dictionary = Dictionary::new(include_str!("data/thaidict.txt"));
        const LAO: Dictionary = Dictionary::new(include_str!("data/laodict.txt"));
        const KHMER: Dictionary = Dictionary::new(include_str!("data/khmerdict.txt"));
        const MYANMAR: Dictionary = Dictionary::new(include_str!("data/burmesedict.txt"));
        match self {
            Self::Thai => THAI,
            Self::Lao => LAO,
            Self::Khmer => KHMER,
            Self::Myanmar => MYANMAR,
        }
    }

    fn can_start(self, c: char) -> bool {
        let c = u32::from(c);
        match self {
            Self::Thai => matches!(c, 0xe01..=0xe2e | 0xe40..=0xe44),
            Self::Lao => matches!(c, 0xe81..=0xeae | 0xedc..=0xedd | 0xec0..=0xec4),
            Self::Khmer => matches!(c, 0x1780..=0x17b3),
            Self::Myanmar => matches!(c, 0x1000..=0x102a),
        }
    }

    fn can_end(self, c: char) -> bool {
        let c = u32::from(c);
        match self {
            Self::Thai => !matches!(c, 0xe31 | 0xe40..=0xe44),
            Self::Lao => !matches!(c, 0xec0..=0xec4),
            Self::Khmer => c != 0x17d2,
            Self::Myanmar => true,
        }
    }
}

#[derive(Default)]
struct Candidates {
    count: usize,
    ends: [usize; 20],
    prefix: usize,
}

fn regional_breaks<'a>(text: &'a str, script: Regional, emit: &mut impl FnMut(&'a str)) {
    let offsets: Vec<_> = text
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .collect();
    let len = offsets.len() - 1;
    if len < 4 || (len == 4 && matches!(script, Regional::Thai)) {
        emit(text);
        return;
    }
    let dict = script.dictionary();
    let candidates = |start| {
        let mut result = Candidates::default();
        result.prefix = dict.candidates(text, start, &offsets, |end, _| {
            result.ends[result.count] = end;
            result.count += 1;
        });
        result
    };
    let char_at = |ix: usize| text[offsets[ix]..].chars().next();
    let mut start = 0;
    while start < len {
        let first = candidates(start);
        let first_ends = &first.ends[..first.count];
        let mut end = first_ends.last().copied().unwrap_or(start);
        if first.count > 1 && end < len {
            'lookahead: for &candidate in first_ends.iter().rev() {
                let second = candidates(candidate);
                if second.count == 0 {
                    continue;
                }
                end = candidate;
                for &next in second.ends[..second.count].iter().rev() {
                    if next == len || candidates(next).count != 0 {
                        break 'lookahead;
                    }
                }
            }
        }
        if end < len && end - start < 3 {
            let next = candidates(end);
            if next.count == 0 && (end == start || next.prefix < 3) {
                end += 1;
                while end < len {
                    if char_at(end - 1).is_some_and(|c| script.can_end(c))
                        && char_at(end).is_some_and(|c| script.can_start(c))
                        && candidates(end).count != 0
                    {
                        break;
                    }
                    end += 1;
                }
            }
        }
        end = end.max(start + 1);
        while end < len && MARK.is_match(&text[offsets[end]..]) {
            end += 1;
        }
        if matches!(script, Regional::Thai) && end < len && candidates(end).count == 0 {
            let repeated = char_at(end - 1);
            if char_at(end) == Some('\u{e2f}') && !matches!(repeated, Some('\u{e2f}' | '\u{e46}')) {
                end += 1;
            }
            if char_at(end) == Some('\u{e46}') && char_at(end - 1) != Some('\u{e46}') {
                end += 1;
            }
        }
        emit(&text[offsets[start]..offsets[end]]);
        start = end;
    }
}
