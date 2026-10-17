//! Word segments like ICU4C's word break iterator: Node's `Intl.Segmenter(undefined, { granularity: "word" })`,
//! which tui-alt-screen.ts `getWordSelection` uses for double-click selection.
//!
//! Two layers, as in ICU4C:
//! - The word rules (ICU's word.txt: UAX #29 tailored so that Han, Hiragana and Katakana chain into one run, Hangul
//!   into another, and complex-context letters such as Thai count as ALetter), with each rule's status deciding
//!   `isWordLike` (status >= 100).
//! - Rule segments holding dictionary characters are subdivided by ICU4C's break engines over the ICU dictionaries
//!   (the same cjdict/thaidict/laodict/khmerdict/burmesedict tries, shipped by `icu_segmenter_data`): CJ text by
//!   `CjkBreakEngine`'s best path over word costs (with the Katakana run cost), Thai, Lao, Khmer and Burmese by the
//!   `ThaiBreakEngine` family's three-word lookahead. ICU4X's own segmenter is not used: its dictionary segmenter is
//!   a greedy longest match and its word rules differ (Katakana and Thai runs, single-character statuses).

use icu_collections::char16trie::{Char16Trie, TrieResult};
use icu_normalizer::ComposingNormalizerBorrowed;
use icu_properties::CodePointMapData;
use icu_properties::CodePointSetData;
use icu_properties::props::{
    CanonicalCombiningClass, ExtendedPictographic, GeneralCategory, GeneralCategoryGroup, GraphemeClusterBreak,
    Ideographic, LineBreak, Script, WordBreak,
};
use icu_provider::prelude::*;
use icu_segmenter::provider::{Baked, SegmenterDictionaryAutoV1, SegmenterDictionaryExtendedV1};

fn script(c: char) -> Script {
    CodePointMapData::<Script>::new().get(c)
}

fn is_sa(c: char) -> bool {
    CodePointMapData::<LineBreak>::new().get(c) == LineBreak::ComplexContext
}

fn is_mark(c: char) -> bool {
    GeneralCategoryGroup::Mark.contains(CodePointMapData::<GeneralCategory>::new().get(c))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Engine {
    Cj,
    Thai,
    Lao,
    Burmese,
    Khmer,
}

impl Engine {
    fn for_char(c: char) -> Option<Engine> {
        [Engine::Cj, Engine::Thai, Engine::Lao, Engine::Burmese, Engine::Khmer]
            .into_iter()
            .find(|e| e.contains(c))
    }

    /// The engine's character set (`setCharacters`).
    fn contains(self, c: char) -> bool {
        match self {
            Engine::Cj => {
                matches!(script(c), Script::Han | Script::Hiragana | Script::Katakana)
                    || matches!(c, '\u{30FC}' | '\u{FF70}' | '\u{FF9E}' | '\u{FF9F}')
            }
            Engine::Thai => script(c) == Script::Thai && is_sa(c),
            Engine::Lao => script(c) == Script::Lao && is_sa(c),
            Engine::Burmese => script(c) == Script::Myanmar && is_sa(c),
            Engine::Khmer => script(c) == Script::Khmer && is_sa(c),
        }
    }

    fn dictionary(self) -> &'static str {
        match self {
            Engine::Cj => "cjdict",
            Engine::Thai => "thaidict",
            Engine::Lao => "laodict",
            Engine::Burmese => "burmesedict",
            Engine::Khmer => "khmerdict",
        }
    }
}

/// A rule `$dictionary` character: complex context (line break SA) or Han/Hiragana/Katakana.
fn is_dictionary_char(c: char) -> bool {
    let k = class(c);
    is_sa(c) || k.kana_kanji || k.hangul
}

fn with_trie<R>(engine: Engine, f: impl FnOnce(&Char16Trie<'_>) -> R) -> R {
    let attrs = DataMarkerAttributes::from_str_or_panic(engine.dictionary());
    let req = DataRequest {
        id: DataIdentifierBorrowed::for_marker_attributes(attrs),
        ..Default::default()
    };
    if engine == Engine::Cj {
        let resp: DataResponse<SegmenterDictionaryAutoV1> = Baked.load(req).expect("cjdict");
        f(&Char16Trie::new(resp.payload.get().trie_data.clone()))
    } else {
        let resp: DataResponse<SegmenterDictionaryExtendedV1> = Baked.load(req).expect("dict");
        f(&Char16Trie::new(resp.payload.get().trie_data.clone()))
    }
}

/// `DictionaryMatcher::matches` from `text[start..]`: (code-point length, value) of each dictionary word that
/// starts there (at most `limit`), stopping once `max_length` code units are matched; plus the code points looked at.
fn matches(
    trie: &Char16Trie<'_>,
    text: &[char],
    start: usize,
    max_length: usize,
    limit: usize,
) -> (Vec<(usize, usize, i32)>, usize) {
    let mut iter = trie.iter();
    let mut words = Vec::new();
    let (mut units, mut points) = (0, 0);
    for &c in &text[start..] {
        let result = iter.next32(u32::from(c));
        units += c.len_utf16();
        points += 1;
        match result {
            TrieResult::Intermediate(v) | TrieResult::FinalValue(v) => {
                if words.len() < limit {
                    words.push((points, units, v));
                }
                if matches!(result, TrieResult::FinalValue(_)) {
                    break;
                }
            }
            TrieResult::NoMatch => break,
            TrieResult::NoValue => {}
        }
        if units >= max_length {
            break;
        }
    }
    (words, points)
}

fn is_katakana(c: char) -> bool {
    let c = u32::from(c);
    (0x30A1..=0x30FE).contains(&c) && c != 0x30FB || (0xFF66..=0xFF9F).contains(&c)
}

fn katakana_cost(length: usize) -> u32 {
    const COST: [u32; 9] = [8192, 984, 408, 240, 204, 252, 300, 372, 480];
    COST.get(length).copied().unwrap_or(8192)
}

/// NFKC `range` in fragments that start at a normalization boundary: the normalized code points, each mapped to the
/// index (in `chars`) of its fragment's start, plus the range end.
fn normalize(chars: &[char], start: usize, end: usize) -> (Vec<char>, Vec<usize>) {
    let nfkc = ComposingNormalizerBorrowed::new_nfkc();
    let range: String = chars[start..end].iter().collect();
    if nfkc.is_normalized(&range) {
        return (chars[start..end].to_vec(), (start..=end).collect());
    }
    let (mut text, mut map) = (Vec::new(), Vec::new());
    let mut i = start;
    while i < end {
        let fragment_start = i;
        let mut fragment = String::new();
        fragment.push(chars[i]);
        i += 1;
        while i < end && !has_boundary_before(chars[i]) {
            fragment.push(chars[i]);
            i += 1;
        }
        for c in nfkc.normalize(&fragment).chars() {
            text.push(c);
            map.push(fragment_start);
        }
    }
    map.push(end);
    (text, map)
}

/// `Normalizer2::hasBoundaryBefore` for NFKC, for the characters of a CJ range: `c` normalizes to a starter.
/// (The backward-combining starters, Hangul vowels and a few Indic vowel signs, never occur in a CJ range.)
fn has_boundary_before(c: char) -> bool {
    let nfkc = ComposingNormalizerBorrowed::new_nfkc();
    let mut buf = [0u8; 4];
    nfkc.normalize(c.encode_utf8(&mut buf))
        .chars()
        .next()
        .is_none_or(|first| {
            CodePointMapData::<CanonicalCombiningClass>::new().get(first) == CanonicalCombiningClass::NotReordered
        })
}

/// `CjkBreakEngine::divideUpDictionaryRange`: the best path over dictionary word costs.
fn divide_cj(trie: &Char16Trie<'_>, chars: &[char], start: usize, end: usize, breaks: &mut Vec<usize>) {
    if start >= end {
        return;
    }
    let (text, map) = normalize(chars, start, end);
    let n = text.len();
    let mut best = vec![u32::MAX; n + 1];
    best[0] = 0;
    let mut prev = vec![usize::MAX; n + 1];
    let mut prev_katakana = false;
    for i in 0..n {
        if best[i] == u32::MAX {
            continue;
        }
        let (words, _) = matches(trie, &text, i, 20, n);
        let mut words: Vec<(usize, u32)> = words.into_iter().map(|(cp, _, v)| (cp, v as u32)).collect();
        if (words.is_empty() || words[0].0 != 1) && !('\u{AC00}'..='\u{D7A3}').contains(&text[i]) {
            words.push((1, 255));
        }
        for (length, value) in words {
            let snlp = best[i] + value;
            if snlp < best[i + length] {
                best[i + length] = snlp;
                prev[i + length] = i;
            }
        }
        let katakana = is_katakana(text[i]);
        if !prev_katakana && katakana {
            let mut run = 1;
            while i + run < n && run < 20 && is_katakana(text[i + run]) {
                run += 1;
            }
            if run < 20 {
                let snlp = best[i] + katakana_cost(run);
                if snlp < best[i + run] {
                    best[i + run] = snlp;
                    prev[i + run] = i;
                }
            }
        }
        prev_katakana = katakana;
    }
    let mut tentative = Vec::new();
    if best[n] == u32::MAX {
        tentative.push(n);
    } else {
        let mut i = n;
        while i > 0 {
            tentative.push(i);
            i = prev[i];
        }
    }
    if breaks.last().is_none_or(|&b| b < start) {
        tentative.push(0);
    }
    let mut previous: Option<usize> = None;
    for &cp in tentative.iter().rev() {
        let pos = map[cp];
        if previous.is_none_or(|p| pos > p) && pos != start {
            breaks.push(pos);
        }
        previous = Some(pos);
    }
    if breaks.last() == Some(&end) {
        breaks.pop();
    }
}

#[derive(Default, Clone)]
struct PossibleWord {
    count: usize,
    prefix: usize,
    offset: Option<usize>,
    mark: usize,
    current: usize,
    lengths: Vec<usize>,
}

impl PossibleWord {
    fn candidates(&mut self, trie: &Char16Trie<'_>, text: &[char], pos: &mut usize, end: usize) -> usize {
        let start = *pos;
        if self.offset != Some(start) {
            self.offset = Some(start);
            let (words, prefix) = matches(trie, text, start, end - start, 20);
            self.lengths = words.into_iter().map(|(_, cu, _)| cu).collect();
            self.count = self.lengths.len();
            self.prefix = prefix;
        }
        if self.count > 0 {
            *pos = start + self.lengths[self.count - 1];
        }
        self.current = self.count.wrapping_sub(1);
        self.mark = self.current;
        self.count
    }

    fn accept_marked(&self, pos: &mut usize) -> usize {
        *pos = self.offset.unwrap_or(0) + self.lengths[self.mark];
        self.lengths[self.mark]
    }

    fn back_up(&mut self, pos: &mut usize) -> bool {
        if self.current > 0 && self.current != usize::MAX {
            self.current -= 1;
            *pos = self.offset.unwrap_or(0) + self.lengths[self.current];
            return true;
        }
        false
    }

    fn mark_current(&mut self) {
        self.mark = self.current;
    }
}

struct SaRules {
    root_combine: usize,
    prefix_combine: usize,
    suffixes: bool,
    begin: fn(char) -> bool,
    end: fn(char) -> bool,
}

fn sa_rules(engine: Engine) -> SaRules {
    match engine {
        Engine::Thai => SaRules {
            root_combine: 3,
            prefix_combine: 3,
            suffixes: true,
            begin: |c| ('\u{0E01}'..='\u{0E2E}').contains(&c) || ('\u{0E40}'..='\u{0E44}').contains(&c),
            end: |c| Engine::Thai.contains(c) && c != '\u{0E31}' && !('\u{0E40}'..='\u{0E44}').contains(&c),
        },
        Engine::Lao => SaRules {
            root_combine: 3,
            prefix_combine: 3,
            suffixes: false,
            begin: |c| {
                ('\u{0E81}'..='\u{0EAE}').contains(&c)
                    || ('\u{0EDC}'..='\u{0EDD}').contains(&c)
                    || ('\u{0EC0}'..='\u{0EC4}').contains(&c)
            },
            end: |c| Engine::Lao.contains(c) && !('\u{0EC0}'..='\u{0EC4}').contains(&c),
        },
        Engine::Burmese => SaRules {
            root_combine: 3,
            prefix_combine: 3,
            suffixes: false,
            begin: |c| ('\u{1000}'..='\u{102A}').contains(&c),
            end: |c| Engine::Burmese.contains(c),
        },
        _ => SaRules {
            root_combine: 3,
            prefix_combine: 3,
            suffixes: false,
            begin: |c| ('\u{1780}'..='\u{17B3}').contains(&c),
            end: |c| Engine::Khmer.contains(c) && c != '\u{17D2}',
        },
    }
}

/// `ThaiBreakEngine` (and the Lao, Burmese and Khmer engines) `divideUpDictionaryRange`.
fn divide_sa(
    engine: Engine,
    trie: &Char16Trie<'_>,
    text: &[char],
    range_start: usize,
    range_end: usize,
    breaks: &mut Vec<usize>,
) {
    const LOOKAHEAD: usize = 3;
    // "Not enough characters for two words": Thai moves 4 code points from the range start and needs to land
    // before its end; the Lao, Burmese and Khmer engines compare the range length with 4.
    let too_short = if engine == Engine::Thai {
        range_start + 4 >= range_end
    } else {
        range_end - range_start < 4
    };
    if too_short {
        return;
    }
    let rules = sa_rules(engine);
    let mark = |c: char| c == ' ' || engine.contains(c) && is_mark(c);
    let mut words: [PossibleWord; LOOKAHEAD] = Default::default();
    let mut found = 0usize;
    let mut pos = range_start;
    let at = |p: usize| text.get(p).copied();
    while pos < range_end {
        let current = pos;
        let mut cp_length = 0;
        let mut cu_length = 0;
        let candidates = words[found % LOOKAHEAD].candidates(trie, text, &mut pos, range_end);
        if candidates == 1 {
            cu_length = words[found % LOOKAHEAD].accept_marked(&mut pos);
            cp_length = cu_length;
            found += 1;
        } else if candidates > 1 {
            'best: {
                if pos >= range_end {
                    break 'best;
                }
                loop {
                    if words[(found + 1) % LOOKAHEAD].candidates(trie, text, &mut pos, range_end) > 0 {
                        words[found % LOOKAHEAD].mark_current();
                        if pos >= range_end {
                            break 'best;
                        }
                        loop {
                            if words[(found + 2) % LOOKAHEAD].candidates(trie, text, &mut pos, range_end) > 0 {
                                words[found % LOOKAHEAD].mark_current();
                                break 'best;
                            }
                            if !words[(found + 1) % LOOKAHEAD].back_up(&mut pos) {
                                break;
                            }
                        }
                    }
                    if !words[found % LOOKAHEAD].back_up(&mut pos) {
                        break;
                    }
                }
            }
            cu_length = words[found % LOOKAHEAD].accept_marked(&mut pos);
            cp_length = cu_length;
            found += 1;
        }
        if pos < range_end && cp_length < rules.root_combine {
            if words[found % LOOKAHEAD].candidates(trie, text, &mut pos, range_end) == 0
                && (cu_length == 0 || words[found % LOOKAHEAD].prefix < rules.prefix_combine)
            {
                let mut remaining = range_end as isize - (current + cu_length) as isize;
                let mut chars = 0;
                loop {
                    let pc = at(pos);
                    pos += 1;
                    chars += 1;
                    remaining -= 1;
                    if remaining <= 0 {
                        break;
                    }
                    let uc = at(pos);
                    if pc.is_some_and(rules.end) && uc.is_some_and(rules.begin) {
                        let n = words[(found + 1) % LOOKAHEAD].candidates(trie, text, &mut pos, range_end);
                        pos = current + cu_length + chars;
                        if n > 0 {
                            break;
                        }
                    }
                }
                if cu_length == 0 {
                    found += 1;
                }
                cu_length += chars;
            } else {
                pos = current + cu_length;
            }
        }
        while pos < range_end && at(pos).is_some_and(mark) {
            pos += 1;
            cu_length += 1;
        }
        if rules.suffixes && pos < range_end && cu_length > 0 {
            const PAIYANNOI: char = '\u{0E2F}';
            const MAIYAMOK: char = '\u{0E46}';
            let suffix = |c: Option<char>| matches!(c, Some(PAIYANNOI | MAIYAMOK));
            let mut uc = at(pos);
            if words[found % LOOKAHEAD].candidates(trie, text, &mut pos, range_end) == 0 && suffix(uc) {
                if uc == Some(PAIYANNOI) && !suffix(at(pos - 1)) {
                    pos += 1;
                    cu_length += 1;
                    uc = at(pos);
                }
                if uc == Some(MAIYAMOK) && at(pos - 1) != Some(MAIYAMOK) {
                    pos += 1;
                    cu_length += 1;
                }
            } else {
                pos = current + cu_length;
            }
        }
        if cu_length > 0 {
            breaks.push(current + cu_length);
        }
    }
    if breaks.last().is_some_and(|&b| b >= range_end) {
        breaks.pop();
    }
}

/// `DictionaryCache::populateDictionary` over the rule segment `chars[start..end]`: its subdivision boundaries
/// (including `start` and `end`), or None when the dictionaries found no break.
fn populate(chars: &[char], start: usize, end: usize) -> Option<Vec<usize>> {
    let units: usize = chars[start..end].iter().map(|c| c.len_utf16()).sum();
    if units <= 1 {
        return None;
    }
    let mut breaks = Vec::new();
    let mut i = start;
    loop {
        while i < end && !is_dictionary_char(chars[i]) {
            i += 1;
        }
        if i >= end {
            break;
        }
        match Engine::for_char(chars[i]) {
            Some(engine) => {
                let mut j = i;
                while j < end && engine.contains(chars[j]) {
                    j += 1;
                }
                with_trie(engine, |trie| {
                    if engine == Engine::Cj {
                        divide_cj(trie, chars, i, j, &mut breaks)
                    } else {
                        divide_sa(engine, trie, chars, i, j, &mut breaks)
                    }
                });
                i = j;
            }
            None => {
                let s = script(chars[i]);
                while i < end && script(chars[i]) == s {
                    i += 1;
                }
            }
        }
    }
    if breaks.is_empty() {
        return None;
    }
    if start < breaks[0] {
        breaks.insert(0, start);
    }
    if end > *breaks.last().unwrap() {
        breaks.push(end);
    }
    Some(breaks)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Class {
    wb: WordBreak,
    /// `$ALetterPlus | $Hebrew_Letter`.
    ah: bool,
    hebrew: bool,
    kana_kanji: bool,
    hangul: bool,
    ex_fm: bool,
    ext_pict: bool,
    /// The status of the single-character rules (`$IdeographicEx`, `$HiraganaEx`, ...): (with trailing ExFm, alone).
    single: (u16, u16),
}

fn class(c: char) -> Class {
    let wb = CodePointMapData::<WordBreak>::new().get(c);
    let sc = script(c);
    let hangul = ('\u{AC00}'..='\u{D7A3}').contains(&c);
    let katakana = wb == WordBreak::Katakana;
    let kana_kanji = sc == Script::Han || sc == Script::Hiragana || katakana;
    let ex_fm = matches!(wb, WordBreak::Extend | WordBreak::Format | WordBreak::ZWJ);
    let control = CodePointMapData::<GraphemeClusterBreak>::new().get(c) == GraphemeClusterBreak::Control;
    let hebrew = wb == WordBreak::HebrewLetter;
    let ah = hebrew
        || (wb == WordBreak::ALetter && !kana_kanji && !hangul)
        || (is_sa(c) && wb != WordBreak::Extend && !control);
    let ideographic = CodePointSetData::new::<Ideographic>().contains(c);
    let single_ex = if ideographic || sc == Script::Hiragana || katakana {
        400
    } else if ah {
        200
    } else if wb == WordBreak::Numeric {
        100
    } else {
        0
    };
    let single = (single_ex, if hangul { 200 } else { single_ex });
    Class {
        wb,
        ah,
        hebrew,
        kana_kanji,
        hangul,
        ex_fm,
        ext_pict: CodePointSetData::new::<ExtendedPictographic>().contains(c),
        single,
    }
}

fn is_mid_letter(k: &Class) -> bool {
    k.wb == WordBreak::MidLetter || matches!(k.wb, WordBreak::MidNumLet | WordBreak::SingleQuote)
}

fn is_mid_num(k: &Class) -> bool {
    matches!(k.wb, WordBreak::MidNum | WordBreak::MidNumLet | WordBreak::SingleQuote)
}

/// The status of the rule joining base `a` to base `b` across ExFm (`direct`: nothing between them), with the
/// bases around them (`before` a, `after` b) for the MidLetter/MidNum rules; None when no rule joins them.
fn join(k: &[Class], a: usize, b: usize, before: Option<usize>, after: Option<usize>, ri_odd: bool) -> Option<u16> {
    let (ka, kb) = (&k[a], &k[b]);
    let direct = b == a + 1;
    if direct && kb.ext_pict && ka.wb == WordBreak::ZWJ {
        return Some(0);
    }
    if direct && ka.wb == WordBreak::WSegSpace && kb.wb == WordBreak::WSegSpace {
        return Some(0);
    }
    let num = |x: &Class| x.wb == WordBreak::Numeric;
    let kata = |x: &Class| x.wb == WordBreak::Katakana;
    let enl = |x: &Class| x.wb == WordBreak::ExtendNumLet;
    if ka.ah && kb.ah {
        return Some(200);
    }
    if ka.ah && is_mid_letter(kb) && after.is_some_and(|c| k[c].ah) {
        return Some(200);
    }
    if is_mid_letter(ka) && kb.ah && before.is_some_and(|c| k[c].ah) {
        return Some(200);
    }
    if ka.hebrew && kb.wb == WordBreak::SingleQuote {
        return Some(200);
    }
    if ka.hebrew && kb.wb == WordBreak::DoubleQuote && after.is_some_and(|c| k[c].hebrew) {
        return Some(200);
    }
    if ka.wb == WordBreak::DoubleQuote && kb.hebrew && before.is_some_and(|c| k[c].hebrew) {
        return Some(200);
    }
    if num(ka) && num(kb) {
        return Some(100);
    }
    if ka.ah && num(kb) || num(ka) && kb.ah {
        return Some(200);
    }
    if num(ka) && is_mid_num(kb) && after.is_some_and(|c| num(&k[c])) {
        return Some(100);
    }
    if is_mid_num(ka) && num(kb) && before.is_some_and(|c| num(&k[c])) {
        return Some(100);
    }
    if kata(ka) && kata(kb) {
        return Some(400);
    }
    if enl(kb) && (ka.ah || enl(ka)) {
        return Some(200);
    }
    if enl(kb) && num(ka) {
        return Some(100);
    }
    if enl(kb) && kata(ka) {
        return Some(400);
    }
    if enl(ka) && kb.ah {
        return Some(200);
    }
    if enl(ka) && num(kb) {
        return Some(100);
    }
    if enl(ka) && kata(kb) {
        return Some(400);
    }
    if ri_odd && ka.wb == WordBreak::RegionalIndicator && kb.wb == WordBreak::RegionalIndicator {
        return Some(0);
    }
    if direct && ka.hangul && kb.hangul {
        return Some(200);
    }
    if direct && ka.kana_kanji && kb.kana_kanji {
        return Some(400);
    }
    None
}

/// ICU's word rules (word.txt): the rule segments of `chars` as (start, end, status).
fn rule_segments(chars: &[char]) -> Vec<(usize, usize, u16)> {
    let k: Vec<Class> = chars.iter().map(|&c| class(c)).collect();
    let newline = |i: usize| matches!(k[i].wb, WordBreak::CR | WordBreak::LF | WordBreak::Newline);
    let n = chars.len();
    let mut out = Vec::new();
    let mut start = 0;
    while start < n {
        // The segment's first base character (None: a leading ExFm run, `^$ExFm+`).
        let mut i = start;
        let mut base: Option<usize>;
        let mut prev_base: Option<usize> = None;
        let mut ri_count = 0;
        if newline(i) {
            i += 1;
            if k[start].wb == WordBreak::CR && i < n && k[i].wb == WordBreak::LF {
                i += 1;
            }
            out.push((start, i, 0));
            start = i;
            continue;
        }
        let mut joined: Option<u16> = None;
        if k[i].ex_fm {
            while i < n && k[i].ex_fm && !(i > start && k[i - 1].wb == WordBreak::ZWJ && k[i].ext_pict) {
                i += 1;
            }
            // `$ZWJ $Extended_Pict` chains onto the leading run.
            if !(i < n && i > start && k[i - 1].wb == WordBreak::ZWJ && k[i].ext_pict) {
                out.push((start, i, 0));
                start = i;
                continue;
            }
            joined = Some(0);
        }
        base = Some(i);
        if k[i].wb == WordBreak::RegionalIndicator {
            ri_count = 1;
        }
        i += 1;
        let mut trailing = 0;
        loop {
            // Attach ExFm (WB4), except ZWJ before Extended_Pictographic which rule 3c handles first.
            while i < n && k[i].ex_fm && !(k[i - 1].wb == WordBreak::ZWJ && k[i].ext_pict) {
                i += 1;
                trailing += 1;
            }
            if i >= n || newline(i) {
                break;
            }
            let b = base.unwrap();
            let next = i;
            // The base after `next` (skipping ExFm) for rules 6/12.
            let mut after = next + 1;
            while after < n && k[after].ex_fm {
                after += 1;
            }
            let after = (after < n).then_some(after);
            let a = if trailing > 0 && k[i - 1].wb == WordBreak::ZWJ && k[next].ext_pict {
                i - 1
            } else {
                b
            };
            let status_join = if a != b {
                join(&k, a, next, None, None, false)
            } else {
                join(&k, b, next, prev_base, after, ri_count % 2 == 1)
            };
            match status_join {
                Some(s) => {
                    prev_base = Some(b);
                    base = Some(next);
                    if k[next].wb == WordBreak::RegionalIndicator {
                        ri_count += 1;
                    } else {
                        ri_count = 0;
                    }
                    joined = Some(s);
                    trailing = 0;
                    i = next + 1;
                }
                None => break,
            }
        }
        let b = base.unwrap();
        // The status of the last rule: a single-character rule (with or without trailing ExFm), or the join into
        // the last base (`{200}`, `{400}`, ...), whichever is larger.
        let status = if trailing > 0 {
            k[b].single.0
        } else {
            k[b].single.1.max(joined.unwrap_or(0))
        };
        out.push((start, i, status));
        start = i;
    }
    out
}

/// The word segments of `line` and whether each is word-like (`isWordLike`), like Node's `Intl.Segmenter`.
pub fn word_segments(line: &str) -> Vec<(&str, bool)> {
    let chars: Vec<char> = line.chars().collect();
    let mut byte_at = Vec::with_capacity(chars.len() + 1);
    let mut b = 0;
    for c in &chars {
        byte_at.push(b);
        b += c.len_utf8();
    }
    byte_at.push(b);
    let mut out = Vec::new();
    for (s, e, status) in rule_segments(&chars) {
        let word_like = status >= 100;
        match populate(&chars, s, e) {
            Some(bounds) => {
                for w in bounds.windows(2) {
                    out.push((&line[byte_at[w[0]]..byte_at[w[1]]], word_like));
                }
            }
            None => out.push((&line[byte_at[s]..byte_at[e]], word_like)),
        }
    }
    out
}
