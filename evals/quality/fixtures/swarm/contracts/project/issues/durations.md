# durations: only the first part of a duration counts

`parse_duration("1h30m")` returns 3600; it should be 5400. Expected behaviour:

- A duration is one or more `<number><unit>` parts, units `d`, `h`, `m`, `s`, each unit at most once and in that
  order (largest first); spaces between parts are allowed: `"2d 4h"`, `"1h30m15s"`, `"45m"`, `"0s"`.
- Anything else raises `ValueError`: an unknown unit (`"5x"`), units out of order or repeated (`"30m1h"`,
  `"1h1h"`), a number without a unit (`"90"`), trailing text (`"1h abc"`), an empty string.
