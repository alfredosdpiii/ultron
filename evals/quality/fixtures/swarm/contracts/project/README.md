# lib

Three small, independent Python modules (standard library only, Python 3.10+):

| Module | What it does |
| --- | --- |
| lib/durations.py | parse human durations such as "1h30m" into seconds |
| lib/ranges.py | merge integer ranges |
| lib/semver.py | compare semantic version strings |

Each has one open bug report under issues/. Quick tests: `python3 -m unittest discover -s tests/quick -t .`

The acceptance check of a module is `python3 check.py <module>` (e.g. `python3 check.py durations`). It prints one
JSON object, `{"module": ..., "passed": true|false, "failures": [...]}`; a fix is done only when it reports
`"passed": true`.
