# calendar: meeting slots

Finds times when every attendee of a meeting is free. All times are integer minutes in UTC; minute 0 is
Monday 00:00 UTC, so `DAY = 1440` and `WEEK = 10080`, and the UTC weekday of minute `m` is `(m // DAY) % 7`
(0 = Monday ... 6 = Sunday). Nothing reads a real clock. Everything is exported from the `slots` package.

## IntervalSet(pairs=())

A set of integers stored as half-open intervals `[start, end)`.

- The constructor takes an iterable of `(start, end)` pairs. Pairs with `end <= start` are dropped. The set is kept
  in canonical form: intervals sorted by start, and any two intervals that overlap **or touch** (`[a, b)` and
  `[b, c)`) are merged, so `intervals()` never returns two intervals with a shared endpoint.
  Example: `IntervalSet([(5, 10), (0, 3), (3, 4), (8, 12), (7, 7)]).intervals() == [(0, 4), (5, 12)]`.
- `IntervalSet.span(start, end)` is `IntervalSet([(start, end)])`.
- `intervals()` returns the canonical intervals as a list of tuples; `len()` counts them; `total()` is the number of
  integers in the set; `bounds()` is `(first, end)` of the whole set or `None` when empty.
- `union(other)`, `intersect(other)` and `subtract(other)` return new sets in canonical form with the integers in
  either set, in both, and in this set but not in `other`.
- `contains(start, end)` is True when every integer of `[start, end)` is in the set; `start >= end` raises
  `ValueError`. `contains_point(m)` tests one integer.
- `clip(start, end)` is the part inside `[start, end)`; `expand(before, after)` widens every interval by `before`
  on the left and `after` on the right (negative widths raise `ValueError`), merging the results;
  `shift(delta)` moves every interval; `longer_than(n)` keeps only intervals of at least `n` integers.

## WeeklyHours(ranges, utc_offset=0)

Working hours in the attendee's local time. Local time is UTC plus `utc_offset` minutes (an int in
`[-720, 840]`, else `ValueError`): local minute `m + utc_offset` corresponds to UTC minute `m`.

- `ranges` maps a weekday (0-6, or a name whose first three letters are `mon` ... `sun`, any case) to a list of
  `(start, end)` clock values: ints (minutes since local midnight) or `"HH:MM"` strings, with
  `0 <= start < end <= 1440` (`"24:00"` is 1440). Anything else raises `ValueError`. Missing weekdays have no
  working time; ranges of one weekday may overlap and are merged.
- `WeeklyHours.office(start="09:00", end="17:00", days=(0, 1, 2, 3, 4), utc_offset=0)` is one range on each day.
- `local_ranges(weekday)` returns the merged local ranges of that weekday.
- `to_utc(start, end)` returns an IntervalSet of the UTC minutes in `[start, end)` that fall in working time:
  UTC minute `m` is working when the local minute `m + utc_offset` lies in a range of its local weekday
  `((m + utc_offset) // DAY) % 7`. Working time can cross UTC midnight and week boundaries after the offset is
  applied. Example: `WeeklyHours({"mon": [("09:00", "17:00")]}, utc_offset=600).to_utc(0, WEEK)` is
  `[(0, 420), (10020, 10080)]`.

## Attendee(name, hours, busy=(), buffer=0)

- `hours` must be a `WeeklyHours` (else `TypeError`); `buffer` a non-negative int (else `ValueError`).
- `busy` is an iterable of `(start, end)` UTC minutes; `add_busy(start, end)` adds one (`end <= start` raises
  `ValueError`); `busy()` returns the merged busy blocks; `clear_busy()` removes them all.
- The buffer keeps `buffer` minutes before and after every busy block free of meetings: minute `m` is **blocked**
  when `s - buffer <= m < e + buffer` for some busy block `[s, e)`.
- `blocked(start, end)` is the IntervalSet of blocked minutes in `[start, end)`.
- `free(start, end)` is the IntervalSet of minutes in `[start, end)` that are working and not blocked (empty when
  `end <= start`). `is_free(start, end)` is True when all of `[start, end)` is free.
  Example: with hours all day Monday, busy `[(500, 600)]` and buffer 30, `free(600, 700) == [(630, 700)]` and
  `free(400, 500) == [(400, 470)]`.

## Search

- `common_free(attendees, start, end)`: IntervalSet of the minutes of `[start, end)` free for every attendee.
  An empty attendee list raises `ValueError`.
- `free_slots(attendees, start, end, min_length=1)`: the intervals of `common_free(...)` (maximal, canonical) that
  are at least `min_length` minutes long, as a list of tuples. `min_length` must be a positive int.
- `find_slot(attendees, duration, earliest, granularity=1, horizon=DEFAULT_HORIZON)`: the smallest `t` such that
  - `t >= earliest` and `t` is a multiple of `granularity` (`t % granularity == 0`),
  - every minute of `[t, t + duration)` is free for every attendee, and
  - `t + duration <= earliest + horizon`;

  or `None` if there is no such `t`. `DEFAULT_HORIZON` is four weeks. `duration`, `granularity` and `horizon`
  must be positive ints and the list non-empty, else `ValueError`. The search crosses day and week boundaries.
  Example: office hours Monday to Friday 09:00-17:00 UTC, busy `[(540, 600)]`: `find_slot([a], 30, 0, 15) == 600`,
  and an hour from Friday 16:45 (`4 * DAY + 1005`) is next Monday 09:00,
  `WEEK + 540`.
- `find_slots(attendees, duration, earliest, count, granularity=1, horizon=DEFAULT_HORIZON)`: up to `count` slots
  `(start, end)`, each the first found at or after the previous slot's end, all ending by `earliest + horizon`.
- `first_common_day(attendees, earliest, min_minutes, horizon=DEFAULT_HORIZON)`: the start of the first UTC day
  (starting with the day of `earliest`, counting only minutes from `earliest` on) with at least `min_minutes`
  common free minutes, or `None` within the horizon.

## Helpers

`align_up(value, granularity)` is the smallest multiple of `granularity` that is `>= value`; `weekday_of(m)` is
`(m // DAY) % 7`; `format_minute(m)` renders a label such as `"w1 tue 09:30"`.

## Checks

`python3 harness.py` (from this directory) runs the service's checks; see the harness's docstring.
