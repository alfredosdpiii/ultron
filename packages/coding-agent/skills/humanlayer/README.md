# HumanLayer skills for Ultron

`diagram-it` is ported from HumanLayer's [`show-me`](https://github.com/humanlayer/skills/tree/main/plugins/show-me)
skill (`npx skills add humanlayer/skills --skill show-me`). The view catalogue is theirs; the port renames it, makes
its views terminal-first (box drawings and trees by default, Mermaid when it will be rendered), builds diagrams from
the code in the REPL and checks every name exists, and only opens an HTML page when a display is available.

HumanLayer's skills are MIT licensed; the license is [LICENSE](LICENSE) in this directory.

Use: `/skill:diagram-it` (or ask Ultron to "diagram this"). A skill named `diagram-it` in your own skills directories
replaces this one.
