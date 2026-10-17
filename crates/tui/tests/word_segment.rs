//! Word segments like Node's ICU `Intl.Segmenter` (word granularity), which TypeScript's double-click selection
//! uses: replays fixtures/word-segments-parity.json (generate-word-segments.mjs: paragraphs, 1,500 seeded random
//! lines, 1,000 Thai/Lao/Khmer/Burmese lines and long katakana words) and the CJK lines of fixtures/fullscreen-clicks-parity.json through `tui::word_segment`, plus literal
//! cases recorded from Node 26 (ICU 78.3) for the dictionary engines and the rule statuses.

use serde_json::Value;
use tui::word_segment::word_segments;

/// A line's segments as `text` for word-like ones, `·text` otherwise, joined by `|`.
fn shown(line: &str) -> String {
    word_segments(line)
        .into_iter()
        .map(|(s, word)| if word { s.to_string() } else { format!("·{s}") })
        .collect::<Vec<_>>()
        .join("|")
}

fn recorded(case: &Value) -> String {
    case["segments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            let text = s[0].as_str().unwrap();
            if s[1].as_bool().unwrap() {
                text.to_string()
            } else {
                format!("·{text}")
            }
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn replay(cases: &[Value]) -> usize {
    for case in cases {
        let line = case["line"].as_str().unwrap();
        assert_eq!(shown(line), recorded(case), "{line:?}");
    }
    cases.len()
}

#[test]
fn tui_word_segment_matches_node_icu_on_the_recorded_lines() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/word-segments-parity.json")).unwrap();
    assert_eq!(fixture["icu"], "78.3");
    assert_eq!(replay(fixture["paragraphs"].as_array().unwrap()), 14);
    assert_eq!(replay(fixture["random"].as_array().unwrap()), 1500);
    assert_eq!(replay(fixture["complex"].as_array().unwrap()), 1000);
    assert_eq!(replay(fixture["katakana"].as_array().unwrap()), 18);
}

#[test]
fn tui_word_segment_matches_node_icu_on_the_click_fixture_lines() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/fullscreen-clicks-parity.json")).unwrap();
    assert_eq!(replay(fixture["words"].as_array().unwrap()), 50);
}

/// `CjkBreakEngine`: the cheapest path over cjdict word costs, a run of katakana costed by its length, single
/// characters at 255, and the input NFKC-normalized (half-width kana, squared words) with breaks mapped back.
#[test]
fn tui_word_segment_cj_best_path_like_icu() {
    let cases = [
        ("東京都に住んでいます", "東京|都|に|住|んで|い|ます"),
        ("中文分词测试很重要", "中文|分词|测试|很|重要"),
        ("ウルトラマンタロウ", "ウルトラマン|タ|ロ|ウ"),
        ("キャッシュメモリ", "キャッシュメモリ"),
        ("カタカナーーー長音", "カタカナーーー|長音"),
        ("ﾃﾞｰﾀﾍﾞｰｽ接続", "ﾃﾞｰﾀﾍﾞｰｽ|接続"),
        ("㌀アパート", "㌀|アパート"),
        ("一ヶ月前", "一|ヶ月|前"),
        // The break at a dictionary range's end is dropped: the rest of the rule segment joins the last word.
        ("コンパイルエラー_テスト", "コンパイル|エラー_テスト"),
        // Han + variation selector is its own rule segment.
        ("葛\u{E0100}城市", "葛\u{E0100}|城市"),
    ];
    for (line, want) in cases {
        assert_eq!(shown(line), want, "{line}");
    }
}

/// The Thai, Lao and Burmese engines: three-word lookahead, Thai letters joining Latin in one rule segment,
/// PAIYANNOI and MAIYAMOK suffixes.
#[test]
fn tui_word_segment_south_east_asian_engines_like_icu() {
    let cases = [
        ("ภาษาไทยไม่มีช่องว่างระหว่างคำ", "ภาษา|ไทย|ไม่มี|ช่อง|ว่าง|ระหว่าง|คำ"),
        ("abcภาษาไทยdef", "abcภาษา|ไทยdef"),
        ("กรุงเทพฯ ดีๆ", "กรุงเทพฯ|· |ดีๆ"),
        ("ສະບາຍດີ ພາສາລາວ", "ສະບາຍດີ|· |ພາສາ|ລາວ"),
        ("မြန်မာစာ မင်္ဂလာပါ", "မြန်မာ|စာ|· |မင်္ဂလာ|ပါ"),
    ];
    for (line, want) in cases {
        assert_eq!(shown(line), want, "{line}");
    }
}

/// `isWordLike` is the status of the last rule: 々 alone has none (not Ideographic), 〆 has; Hangul with a
/// combining mark has none; a ZWJ emoji sequence after a letter has none; ExtendNumLet alone has none.
#[test]
fn tui_word_segment_rule_statuses_like_icu() {
    assert_eq!(shown("々 〆 _ a\u{200D}🙂"), "·々|· |〆|· |·_|· |·a\u{200D}🙂");
    assert_eq!(shown("한\u{0301}한 한a"), "·한\u{0301}|한|· |한|a");
}
