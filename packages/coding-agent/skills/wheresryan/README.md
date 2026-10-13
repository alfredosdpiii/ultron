# Anatomy for Ultron

`anatomy` is Ryan's ([@wheresryan22](https://github.com/wheresryan22)) [Anatomy](https://github.com/wheresryan22/anatomy)
skill (commit `bb13e5e`; [skills.wheresryan.sh/anatomy](https://skills.wheresryan.sh/anatomy)). Its rule is "the object
is the explanation": the model invents a real machine whose motion is the idea and draws it as a live isometric SVG/HTML
figure, built from many small solid parts with the bundled kit (`anatomy/kit/`), then checks it in headless Chrome
(`anatomy/scripts/`). Figures are sized as a Figure, a Hero or an Epic, and an Epic is never started without the user.

The kit, the scripts and the example sources are upstream's, unmodified; they use only Node.js built-ins (20.10+) and a
local Chrome or Chromium for the visual checks. The port changes only the instructions:

- commands run from REPL cells (`await bash(...)`), screenshots are looked at with `view_image`, and long audits run as
  jobs (`bash(..., yield_after=0)`) instead of shell background runs;
- a first step checks for Node 20.10+ and a Chrome or Chromium binary (passed to the scripts as `CHROME_PATH`) and, with
  none, tells the user plainly that the figure can be built and audited but not looked at;
- figures are built outside the skill's directory, and an example is built in a scratch copy with `kit/` and
  `scripts/` linked, since the prebuilt example pages (about 11 MB) are not bundled;
- publishing to the community gallery passes `--agent Other --no-session` (the session reader knows only Claude Code's
  logs) and is still done only when the user asks and says yes;
- upstream lets agents reach for the skill on their own; here, like the other bundled third-party skills, it has
  `disable-model-invocation: true` and is read only when asked for.

Left out: the prebuilt example pages, upstream's own tests (`scripts/*.test.mjs`, `tests/fixtures/`), `evals/`,
`agents/openai.yaml`, `.github/`, and the repository's README, CHANGELOG, CODE_OF_CONDUCT, CONTRIBUTING and SECURITY
files.

Anatomy is MIT licensed; the license is [LICENSE](LICENSE) in this directory.

Use: `/skill:anatomy <what to explain>`. A skill named `anatomy` in your own skills directories replaces this one.
