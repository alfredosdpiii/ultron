// Generates crates/tui/fixtures/word-segments-parity.json: Node's `Intl.Segmenter(undefined, { granularity: "word" })`
// (ICU, the segmenter tui-alt-screen.ts getWordSelection uses) over
// - `paragraphs`: running text in Japanese, Chinese, Thai, Lao, Khmer and Burmese, long katakana runs and half-width
//   forms (the dictionary engines over long ranges), and
// - `random`: lines drawn by a seeded LCG from pools of word-break-relevant characters (kanji/kana incl. iteration
//   marks, half-width and compatibility forms; Latin, digits, ExtendNumLet, MidLetter/MidNum punctuation, spaces,
//   CJK punctuation; combining marks, ZWJ, variation selectors; emoji, flags; Hangul; Thai, Lao, Khmer, Burmese,
//   Hebrew; newlines), switching pool now and then so that scripts meet,
// - `complex`: seeded lines per Thai, Lao, Khmer and Burmese from pools of real words, consonants, vowel signs and
//   tone marks (the engines' lookahead, resynchronisation, combining-mark and suffix rules), and
// - `katakana`: long katakana words, some longer than ten characters (the 20-unit dictionary match limit and the
//   katakana run cost).
// Each line records its segments as [text, isWordLike]. crates/tui/tests/word_segment.rs replays them through
// tui::word_segment. Deterministic: run it twice and compare sha256.
// Usage: node crates/tui/fixtures/generate-word-segments.mjs
import "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const PARAGRAPHS = [
	"吾輩は猫である。名前はまだ無い。どこで生れたかとんと見当がつかぬ。何でも薄暗いじめじめした所でニャーニャー泣いていた事だけは記憶している。",
	"国境の長いトンネルを抜けると雪国であった。夜の底が白くなった。信号所に汽車が止まった。",
	"中华人民共和国是工人阶级领导的、以工农联盟为基础的人民民主专政的社会主义国家。",
	"学而时习之，不亦说乎？有朋自远方来，不亦乐乎？人不知而不愠，不亦君子乎？",
	"ประเทศไทยตั้งอยู่ในเอเชียตะวันออกเฉียงใต้ มีพรมแดนติดต่อกับประเทศลาวและกัมพูชาทางทิศตะวันออก",
	"แม่น้ำเจ้าพระยาเป็นแม่น้ำสายสำคัญที่ไหลผ่านกรุงเทพมหานคร",
	"ຂ້ອຍຮັກພາສາລາວຫຼາຍທີ່ສຸດ ປະເທດລາວຕັ້ງຢູ່ໃນອາຊີຕາເວັນອອກສຽງໃຕ້",
	"ព្រះរាជាណាចក្រកម្ពុជាជាប្រទេសមួយនៅអាស៊ីអាគ្នេយ៍",
	"ပြည်ထောင်စုသမ္မတမြန်မာနိုင်ငံတော်သည်အရှေ့တောင်အာရှတွင်တည်ရှိသည်",
	"アンチディスエスタブリッシュメンタリアニズムアンチディスエスタブリッシュメンタリアニズム",
	"カタカナカタカナカタカナカタカナカタカナカタカナ",
	"ﾌﾟﾛｸﾞﾗﾐﾝｸﾞ言語ｼﾞｬﾊﾞｽｸﾘﾌﾟﾄ",
	"Rustで書かれたターミナルUIは、ダブルクリックで単語を選択します（例：「日本語」）。",
	"使用 `cargo test --workspace` 运行全部测试，然后检查输出中的 FAILED。",
];
const POOLS = [
	"日本語東京都中文分词测试漢字一二三人工智能的是了々〆〇豈⼀",
	"ひらがなのはをにでとすもうちゝゞゟ",
	"カタカナコンパイルエラーデータベースヶヵーヽヾ゛゜・ｶﾀｶﾅﾞﾟｰ㋐㌀",
	"abcXYZéß",
	"0123４５",
	"_",
	".",
	"'",
	":",
	",",
	'"',
	" ",
	"　",
	"-",
	"/",
	"。、「」（）！？",
	"゙゚́️‍​\u{E0100}",
	"🙂👍🏽👨‍👩‍👧🇯🇵🇺",
	"한국어단어",
	"ภาษาไทยสวัสดีครับ็่้ๆฯ",
	"ສະບາຍດີລາວ",
	"ភាសាខ្មែរ",
	"မြန်မာစာ",
	"אבגְ",
	"\n\r\t",
];
let seed = 20261017;
const next = (n) => {
	seed = (seed * 1103515245 + 12345) % 2147483648;
	return seed % n;
};
const random = [];
for (let t = 0; t < 1500; t++) {
	const length = 1 + next(t < 1000 ? 12 : 40);
	let pool = [...POOLS[next(POOLS.length)]];
	let line = "";
	for (let i = 0; i < length; i++) {
		if (next(4) === 0) pool = [...POOLS[next(POOLS.length)]];
		line += pool[next(pool.length)];
	}
	random.push(line);
}
const COMPLEX = [
	["ภาษาไทยไม่มีช่องว่างระหว่างคำการเขียนโปรแกรมคอมพิวเตอร์ประเทศไทยมีประชากรมาก", "่้๊๋็ั์ิีึืุู", "ฯๆ", "กขคงจฉชซญดตถทธนบปผพภมยรลวศษสหอฮเแโใไ"],
	["ສະບາຍດີພາສາລາວຂ້ອຍຮັກພາສາລາວຫຼາຍທີ່ສຸດປະເທດລາວຕັ້ງຢູ່ໃນອາຊີຕາເວັນອອກສຽງໃຕ້", "່້໊໋ັິີຶືຸູົຼ"],
	["ភាសាខ្មែរសួស្តីព្រះរាជាណាចក្រកម្ពុជាជាប្រទេសមួយនៅអាស៊ីអាគ្នេយ៍", "ក្ខគ្ឃងចឆជឈញដឋឌឍណតថទធនបផពភមយរលវសហឡអ", "ាិីឹឺុូួើឿៀេែៃោៅំះៈ្់៌៍៎៏័"],
	["ပြည်ထောင်စုသမ္မတမြန်မာနိုင်ငံတော်သည်အရှေ့တောင်အာရှတွင်တည်ရှိသည်မင်္ဂလာပါ", "ါာိီုူေဲံ့း္်ျြွှ"],
];
const complex = [];
for (const pools of COMPLEX) {
	for (let t = 0; t < 250; t++) {
		const length = 1 + next(30);
		let pool = [...pools[next(pools.length)]];
		let line = "";
		for (let i = 0; i < length; i++) {
			if (next(5) === 0) pool = [...pools[next(pools.length)]];
			line += pool[next(pool.length)];
		}
		complex.push(line);
	}
}
const KATAKANA = [
	"コンピュータグラフィックス",
	"アプリケーションソフトウェア",
	"オペレーティングシステム",
	"ユーザーインターフェース",
	"コミュニケーションツール",
	"インフォメーションテクノロジー",
	"プロフェッショナル",
	"エンターテインメント",
	"クリスマスプレゼント",
	"ソーシャルネットワーキングサービス",
	"ミュージックビデオ",
	"ファイナルファンタジー",
	"スーパーマーケット",
	"アイデンティティー",
	"リハビリテーション",
	"インターナショナル",
	"パーソナルコンピュータ",
	"サッカーワールドカップ",
];
const segmenter = new Intl.Segmenter(undefined, { granularity: "word" });
const record = (line) => ({ line, segments: [...segmenter.segment(line)].map((s) => [s.segment, s.isWordLike === true]) });
const fixture = { icu: process.versions.icu, unicode: process.versions.unicode, paragraphs: PARAGRAPHS.map(record), random: random.map(record), complex: complex.map(record), katakana: KATAKANA.map(record) };
// One line per record keeps the file small and its diffs readable.
const list = (records) => `[\n${records.map((r) => `\t\t${JSON.stringify(r)}`).join(",\n")}\n\t]`;
const body = `{\n\t"icu": ${JSON.stringify(fixture.icu)},\n\t"unicode": ${JSON.stringify(fixture.unicode)},\n\t"paragraphs": ${list(fixture.paragraphs)},\n\t"random": ${list(fixture.random)},\n\t"complex": ${list(fixture.complex)},\n\t"katakana": ${list(fixture.katakana)}\n}\n`;
writeFileSync(join(here, "word-segments-parity.json"), body);
console.log(`wrote ${PARAGRAPHS.length} paragraphs, ${random.length} random, ${complex.length} complex-script and ${KATAKANA.length} katakana lines (ICU ${process.versions.icu})`);
