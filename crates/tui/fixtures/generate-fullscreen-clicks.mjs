// Generates crates/tui/fixtures/fullscreen-clicks-parity.json from packages/tui/src/tui-alt-screen.ts and utils.ts
// (TypeScript, this checkout):
//
// - `links`: a TuiAltScreen over raw lines with OSC 8 hyperlinks (ST and BEL terminators, link parameters, a wide
//   emoji, wide CJK text before a link, a tab) gets SGR mouse input: primary clicks with specific and generic release
//   codes, drags, double clicks, other buttons and modifiers. After each step it records the URLs `openUrl` received,
//   the texts copied on select and the screen (previousScreen).
// - `osc8`: getOsc8LinkAtColumn for every column of sample lines.
// - `words`: the double-click word selection (`getWordSelection`, Intl.Segmenter word granularity on Node's ICU,
//   with the `/` and `-` joiners) for every column of CJK sample lines, plus Node's segments for reference.
//
// Timers are virtual and Date.now is a step clock, as in generate-fullscreen-scroll.mjs.
// Usage: node crates/tui/fixtures/generate-fullscreen-clicks.mjs   (deterministic; output is written next to this file)
import "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const timers = [];
const fakeTimer = (interval) => (fn, ms = 0) => {
	const t = { fn, ms, interval, live: true, unref() { return t; }, ref() { return t; }, hasRef() { return false; } };
	timers.push(t);
	return t;
};
globalThis.setTimeout = fakeTimer(false);
globalThis.setInterval = fakeTimer(true);
globalThis.clearTimeout = globalThis.clearInterval = (t) => {
	if (t) t.live = false;
};
let clock = 1_000_000;
Date.now = () => clock;

const here = dirname(fileURLToPath(import.meta.url));
const src = (p) => import(join(here, "../../../packages/tui/src", p));
const { TuiAltScreen } = await src("tui-alt-screen.ts");
const { getOsc8LinkAtColumn, visibleWidth } = await src("utils.ts");
const { setCapabilities } = await src("terminal-image.ts");
setCapabilities({ images: null, trueColor: true, hyperlinks: true });

class Lines {
	constructor(lines) {
		this.lines = lines;
	}
	render() {
		return [...this.lines];
	}
	invalidate() {}
}

class FakeTerminal {
	constructor(columns, rows) {
		this.columns = columns;
		this.rows = rows;
		this.writes = [];
		this.kittyProtocolActive = false;
	}
	start(onInput) {
		this.onInput = onInput;
	}
	stop() {}
	async drainInput() {}
	write(data) {
		this.writes.push(data);
	}
	moveBy() {}
	hideCursor() {}
	showCursor() {}
	clearLine() {}
	clearFromCursor() {}
	clearScreen() {}
	setTitle() {}
	setProgress() {}
}

const st = (text, url, params = "") => `\x1b]8;${params};${url}\x1b\\${text}\x1b]8;;\x1b\\`;
const bel = (text, url) => `\x1b]8;;${url}\x07${text}\x1b]8;;\x07`;
const LINK_LINES = [
	st("link", "https://example.com/path?q=1"),
	bel("link", "https://example.com/bel"),
	st("🙂", "https://example.com/emoji"),
	`日本 ${st("docs", "https://example.com/日本")} x`,
	`a\t${st("tab", "https://example.com/tab", "id=7")}`,
	`see ${st("one", "https://a.example/1")}${st("two", "https://b.example/2")} end`,
	"plain https://example.com/not-a-link",
];

/** Mouse report (0-based cell `x`, `y`). */
const sgr = (button, x, y, release = false) => `\x1b[<${button};${x + 1};${y + 1}${release ? "m" : "M"}`;
const click = (x, y, releaseButton = 0) => [sgr(0, x, y), sgr(releaseButton, x, y, true)];

async function links() {
	timers.length = 0;
	clock = 1_000_000;
	const cols = 24;
	const rows = 8;
	const terminal = new FakeTerminal(cols, rows);
	const opened = [];
	const copies = [];
	const tui = new TuiAltScreen(terminal, false, undefined, {
		openUrl: (url) => opened.push(url),
		copySelection: async (text) => {
			copies.push(text);
			return true;
		},
	});
	tui.addChild(new Lines(LINK_LINES));
	tui.start();
	tui.doRender();
	// Each step is a list of inputs; `at` advances the clock first (clicks far apart are not double clicks).
	const steps = [
		{ name: "click, generic release", inputs: click(1, 0, 3) },
		{ name: "click, specific release", inputs: click(1, 1) },
		{ name: "wide emoji, second cell", inputs: click(1, 2) },
		{ name: "after wide text", inputs: click(6, 3) },
		{ name: "before the link", inputs: click(2, 3) },
		{ name: "after a tab", inputs: click(5, 4) },
		{ name: "tab cell", inputs: click(2, 4) },
		{ name: "adjacent links, first", inputs: click(6, 5) },
		{ name: "adjacent links, second", inputs: click(7, 5) },
		{ name: "plain URL text", inputs: click(10, 6) },
		{ name: "past the end of the line", inputs: click(20, 0) },
		{ name: "drag off the link", inputs: [sgr(0, 1, 0), sgr(32, 3, 0), sgr(0, 3, 0, true)] },
		{ name: "drag back to the press", inputs: [sgr(0, 1, 1), sgr(32, 3, 1), sgr(32, 1, 1), sgr(0, 1, 1, true)] },
		{ name: "release elsewhere", inputs: [sgr(0, 1, 1), sgr(0, 2, 1, true)] },
		{ name: "double click", inputs: [...click(2, 0), ...click(2, 0)] },
		{ name: "triple click", inputs: [...click(2, 1), ...click(2, 1), ...click(2, 1)] },
		{ name: "double click on the first cell of a link", inputs: [...click(0, 2), ...click(0, 2)] },
		{ name: "middle button", inputs: [sgr(1, 1, 0), sgr(1, 1, 0, true)] },
		{ name: "right button", inputs: [sgr(2, 1, 0), sgr(2, 1, 0, true)] },
		{ name: "shift click", inputs: [sgr(4, 1, 0), sgr(4, 1, 0, true)] },
		{ name: "alt click", inputs: [sgr(8, 1, 0), sgr(8, 1, 0, true)] },
		{ name: "ctrl click", inputs: [sgr(16, 1, 0), sgr(16, 1, 0, true)] },
		{ name: "focus lost between press and release", inputs: [sgr(0, 1, 0), "\x1b[O", sgr(0, 1, 0, true)] },
		{ name: "release without press", inputs: [sgr(0, 1, 0, true)] },
		{ name: "click again", inputs: click(1, 0) },
	];
	const frames = [];
	for (const step of steps) {
		clock += 2000;
		for (const input of step.inputs) {
			terminal.onInput(input);
			for (let i = 0; i < 5; i++) await Promise.resolve();
			tui.doRender();
		}
		frames.push({ name: step.name, inputs: step.inputs, opened: [...opened], copies: [...copies], screen: [...tui.previousScreen] });
	}
	tui.stop();
	return { cols, rows, lines: LINK_LINES, frames };
}

function osc8() {
	return LINK_LINES.map((line) => ({
		line,
		links: Array.from({ length: visibleWidth(line) + 2 }, (_, column) => getOsc8LinkAtColumn(line, column) ?? null),
	}));
}

const WORD_LINES = [
	"日本語 テキスト wide",
	"日本語のテキスト",
	"漢字abc漢字",
	"私は学生です",
	"東京都に住んでいます",
	"カタカナとひらがな",
	"コンパイルエラー",
	"データベース接続",
	"中文分词测试很重要",
	"请检查配置文件。下一步",
	"README.mdを更新",
	"src/日本語/main.rs の関数",
	"バージョン2.0 と ｶﾀｶﾅ",
	"〇々〆 ゝゞ ヽヾ ー",
	"한국어 단어 선택",
	"mixed 漢字-kana かな-カナ",
	// Chinese (simplified and traditional), with punctuation.
	"我们今天去北京大学参观了图书馆",
	"这个函数的返回值是一个字符串数组",
	"请在终端中运行以下命令来安装依赖",
	"人工智能正在改变软件开发的方式",
	"繁體中文測試與簡體中文测试",
	"使用Rust编写的程序，运行很快！",
	"𠮷野家で牛丼 𠀋𠂉𠂢𠂤",
	// Japanese: kana/kanji mixes, katakana runs (the katakana cost), half-width and compatibility forms.
	"今日はとても良い天気ですね",
	"この関数はエラーを返すことがあります",
	"ファイルを保存してからビルドしてください",
	"サーバーサイドレンダリングとクライアントサイドレンダリング",
	"東京スカイツリーへ行きました",
	"ウルトラマンタロウ ラーメン屋さん ヴァイオリンを弾く",
	"アイウエオカキクケコサシスセソタチツテトナニヌネノ",
	"すもももももももものうち にわにはにわにわとりがいる",
	"一ヶ月前 一ヵ月 佐々木さん 々木",
	"ﾃﾞｰﾀﾍﾞｰｽ接続 ﾊﾟｿｺﾝ ｱｲｳｴｵ ｶﾞｷﾞｸﾞ",
	"㍿株式会社 ㌀アパート ㋐㋑ ⼀⼆ 豈更 〸〹 ゟより",
	"カタ_カナ コンパイルエラー_テスト 1_コンパイルエラー",
	"コンパイル\u0301エラー 漢\u0301字 葛\u{E0100}城市 カ\u309Aキ\u309A",
	"「引用」と『書名』（括弧）、価格は1,000円です。",
	"ＡＢＣ１２３と全角文字　全角スペース",
	// Korean, alone and next to Han and Latin.
	"안녕하세요 세계 여러분 한국어와日本語と中文 한a 1한 한\u0301한",
	// Thai, Lao, Khmer and Burmese (dictionary engines), and Thai next to Latin and digits.
	"สวัสดีครับ ยินดีต้อนรับ",
	"ภาษาไทยไม่มีช่องว่างระหว่างคำ",
	"การเขียนโปรแกรมคอมพิวเตอร์ ประเทศไทยมีประชากรมาก",
	"abcไทยdef abcภาษาไทยdef ไทย123 ๑๒๓ ภาษา",
	"ดีๆ กรุงเทพฯ ฯลฯ เด็กๆเล่นกัน",
	"ສະບາຍດີ ພາສາລາວ",
	"ភាសាខ្មែរ សួស្តី",
	"မြန်မာစာ မင်္ဂလာပါ",
	// Mixed Latin/CJK, emoji (ZWJ sequences, flags, modifiers).
	"Hello世界こんにちは README.mdを更新",
	"emoji 👨‍👩‍👧‍👦 家族 🇯🇵 日本 👍🏽ok 🙂笑顔 a\u200d🙂",
	"it's a test_case 3.14 foo-bar/baz a:b x.y 1,000",
];

function words() {
	const terminal = new FakeTerminal(80, 4);
	const tui = new TuiAltScreen(terminal, false, undefined, {});
	const segmenter = new Intl.Segmenter(undefined, { granularity: "word" });
	return WORD_LINES.map((line) => {
		tui.previousScreen = [line];
		const width = visibleWidth(line);
		const selections = Array.from({ length: width + 1 }, (_, col) => {
			const range = tui.getWordSelection({ row: 0, col });
			return range ? [range.start.col, range.end.col] : null;
		});
		const segments = [...segmenter.segment(line)].map((s) => [s.segment, s.isWordLike === true]);
		return { line, selections, segments };
	});
}

const fixture = {
	icu: process.versions.icu,
	links: await links(),
	osc8: osc8(),
	words: words(),
};
writeFileSync(join(here, "fullscreen-clicks-parity.json"), `${JSON.stringify(fixture, null, "\t")}\n`);
