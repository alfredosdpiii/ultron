# Ultron skills

Skills written for Ultron itself, as opposed to the ports in the sibling directories.

- `debug-unfamiliar`: facts-first debugging for code nobody here knows well. The agent compares the broken case
  against a control, ranks hypotheses with a hypotheses-by-facts matrix, descends one layer at a time for a strong
  signal, and accepts a cause only when it fits every fact. The method follows Cindy Sridharan's
  [How to effectively debug when you no longer understand the code](https://copyconstruct.medium.com/how-to-effectively-debug-when-you-no-longer-understand-the-code-bd3dcb2c5f7b).

Use: `/skill:<name>`. A skill with the same name in your own skills directories replaces the bundled one.
- `verified-core`: proved specs for small pure logic. The user states laws in `LAWS.bend`, Bend proves a reference
  implementation, and the shipped code is either the compiled Bend (`-o x.mjs` for JS/TS) or tested against it as
  an oracle (any language). `example/` is a checked last-write-wins replay with a 2000-case Python differential
  test. Needs [Bend](https://bend-lang.com); falls back to Hypothesis, fast-check, proptest or CrossHair.
