# no-mistakes for Ultron

`no-mistakes` is Kun Chen's [no-mistakes](https://github.com/kunchenguid/no-mistakes) skill (commit `baa5dff`). It
drives the `no-mistakes` CLI, a local gate that runs committed changes through intent, rebase, review, test, document,
lint, push, PR and CI before they reach the push target. The skill is instructions only: install the CLI and run
`no-mistakes init` in a repository first.

The port keeps the upstream instructions and adds how to run them from the REPL: every command goes through `bash()`,
intent is passed with `--intent-file`, and the long blocking `axi run` / `axi respond` calls become host jobs whose end
arrives as an event instead of being polled. Like the other bundled skills, it is read only when asked for.

no-mistakes is MIT licensed; the license is [LICENSE](LICENSE) in this directory.

Use: `/skill:no-mistakes` to validate committed work, or `/skill:no-mistakes <task>` to do the task and then validate
it. A skill named `no-mistakes` in your own skills directories replaces this one.
