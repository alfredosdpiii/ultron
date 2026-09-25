/**
 * Reference solutions for tasks-judged.mjs, used only by `eval-quality.mjs --tasks judged --self-check`: they prove
 * each deterministic sanity check accepts a good answer, and they are what the rubric wiring (fake judge) and the
 * optional live judge calibration (`--judge-live`) score. Never shown to the agents.
 */

const text = (source) => `${source.replace(/^\n/, "")}\n`;

export const solutions = {
	"refactor-order-report": {
		files: {
			"report.py": text(`
"""Plain-text order report."""

COUPON_PERCENT_OFF = {"SAVE10": 10, "SAVE20": 20}
LARGE_ORDER_THRESHOLD_CENTS = 100_000
LARGE_ORDER_REBATE_CENTS = 500
STATUSES = ("shipped", "pending", "cancelled")


def format_cents(cents):
    """Render integer cents as <units>.<cents>, as the report always has."""
    return f"{cents // 100}.{cents % 100:02d}"


def order_total_cents(order):
    """Item total after the coupon and the large-order rebate."""
    total = sum(item["qty"] * item["unit_cents"] for item in order["items"])
    percent_off = COUPON_PERCENT_OFF.get(order.get("coupon"), 0)
    total -= total * percent_off // 100
    if total > LARGE_ORDER_THRESHOLD_CENTS:
        total -= LARGE_ORDER_REBATE_CENTS
    return total


def order_line(order):
    """One report line for an order; raises ValueError for an unknown status."""
    status = order["status"]
    if status not in STATUSES:
        raise ValueError("bad status " + str(status))
    prefix = f"#{order['id']} {order['customer']} {status}"
    if status == "cancelled":
        return prefix
    return f"{prefix} {format_cents(order_total_cents(order))}"


def render_report(orders):
    """One line per order, then the counts per status and the shipped revenue."""
    lines = []
    counts = dict.fromkeys(STATUSES, 0)
    revenue_cents = 0
    for order in orders:
        lines.append(order_line(order))
        counts[order["status"]] += 1
        if order["status"] == "shipped":
            revenue_cents += order_total_cents(order)
    lines.append(", ".join(f"{status}: {counts[status]}" for status in STATUSES))
    lines.append(f"revenue: {format_cents(revenue_cents)}")
    return "\\n".join(lines)
`),
		},
	},
	"explain-ratelimit-module": {
		files: {
			"EXPLANATION.md": text(`
# The ratelimit package

\`ratelimit\` limits how often something may happen, per caller, while still allowing short bursts. It has two
parts: \`TokenBucket\` (one limiter) and \`BucketRegistry\` (one limiter per key, for example per user or per IP).

## How the token bucket works

A bucket holds up to \`capacity\` tokens and starts **full**. Every action costs tokens (\`cost\`, default 1). Tokens
flow back continuously at \`rate\` tokens per second, but never above \`capacity\`. So a client can burst up to
\`capacity\` actions at once and then sustain \`rate\` actions per second.

The bucket does not run a timer. It refills lazily whenever it is asked (\`_refill\`):

\`\`\`
elapsed = max(0, now - last)
last    = max(last, now)
tokens  = min(capacity, tokens + elapsed * rate)
\`\`\`

\`now\` comes from the injected \`clock\` (default \`time.monotonic\`). Clamping \`elapsed\` at 0 means a clock that
goes backwards adds no tokens, and \`last\` never moves backwards either.

## Public API

- \`TokenBucket(capacity, rate, clock=time.monotonic)\`: both numbers must be positive, otherwise \`ValueError\`.
- \`TokenBucket.allow(cost=1) -> bool\`: refills, then takes \`cost\` tokens and returns True if that many are
  available; otherwise returns False and takes nothing.
- \`TokenBucket.retry_after(cost=1) -> float | None\`: seconds until \`allow(cost)\` could succeed
  (\`(cost - tokens) / rate\`, or 0.0 if it could succeed now), or \`None\` if it never can.
- \`BucketRegistry(capacity, rate, max_keys=10_000, clock=None)\` and \`BucketRegistry.allow(key, cost=1) -> bool\`:
  keeps one \`TokenBucket\` per key, created on first use with the registry's capacity, rate and clock.

## Edge cases the code handles

- **Cost above capacity:** \`allow\` returns False and consumes nothing, forever; \`retry_after\` returns \`None\`.
- **Non-positive values:** \`capacity <= 0\` or \`rate <= 0\` raise \`ValueError\` in the constructor, and
  \`allow\` raises \`ValueError\` for \`cost <= 0\`.
- **Refusals are free:** a refused \`allow\` leaves the tokens unchanged.
- **Clock going backwards:** no refill, no negative tokens.
- **Too many keys:** the registry keeps at most \`max_keys\` buckets. Every \`allow\` pops the key's bucket and
  re-inserts it, so dict order is least recently used first; when a new key arrives at the limit, the oldest key is
  dropped. An evicted key that comes back gets a new, full bucket, so eviction can briefly forgive a heavy user.

## Testing it deterministically

Inject a fake clock instead of sleeping:

\`\`\`python
now = [0.0]
bucket = TokenBucket(capacity=2, rate=1, clock=lambda: now[0])
assert bucket.allow() and bucket.allow() and not bucket.allow()
assert bucket.retry_after() == 1.0
now[0] += 1.0
assert bucket.allow()
assert bucket.retry_after(cost=3) is None
\`\`\`

For the registry, pass the same fake clock and a small \`max_keys\` (for example 2) to check eviction order and that
an evicted key starts full again.
`),
		},
	},
	"design-note-price-cache": {
		files: {
			"DESIGN.md": text(`
# Design note: caching prices in checkout-service

## Goals and non-goals

Goals:
- Cut cart-view latency from pricing-api's 800 ms p50 to a cache lookup for the common case.
- Keep upstream traffic well under pricing-api's 50 requests/s limit at the 400 cart views/s peak.
- Keep checkout working through a pricing-api outage of the length we saw last month (12 minutes) for browsing.
- Never charge a stale price: payment always prices fresh.

Non-goals:
- Changing pricing-api or how prices are computed.
- Caching anything for payment. Payment keeps calling pricing-api directly.

## Proposed design

A two-level read-through cache keyed by SKU:

1. **Shared Redis cache** (\`price:<sku>\` -> price and fetched-at timestamp), used by all 6 instances. Entries have a
   soft TTL of 4 minutes and a hard TTL of 20 minutes.
2. **Per-instance in-memory cache** of the hot set, TTL 30 s, in front of Redis, to absorb the 90% of views that
   touch the same 2k SKUs without a network round trip.

On a cart view the service looks up all SKUs of the cart (1-15) in memory, then Redis with one \`MGET\`. Misses are
collected and fetched from pricing-api in one batch request (up to 50 SKUs), written to Redis, and returned.

Upstream rate check: the hot 2k SKUs refresh at most once per 4 minutes each: 2,000 / 240 s = ~8.3 SKUs/s, which
batched 50 per request is well under 1 request/s. Cold SKUs (28k) are fetched on demand; even if every one were
requested every 4 minutes that is 117 SKUs/s, about 3 batched requests/s. A single refresh job per SKU (see
stampede protection) keeps us an order of magnitude under the 50 requests/s limit, compared with ~400 requests/s
today.

## Invalidation and freshness

- Displayed prices are at most 5 minutes old: the soft TTL (4 minutes) triggers a refresh while the entry is still
  served, which leaves a minute of margin for the refresh to finish.
- Between the soft TTL and the hard TTL an entry is served only when pricing-api is failing, and is marked stale in
  the response so the UI can show "price confirmed at checkout".
- Payment ignores the cache and prices the cart fresh; if that price differs from the displayed one, the user
  confirms the new total before being charged.
- No push invalidation is needed because prices change at most every 5 minutes.

## Failure modes

- **Stampede on expiry:** a refresh takes a short Redis lock (\`SET NX\` with a 5 s expiry) per batch of SKUs; other
  instances keep serving the soft-expired value instead of calling pricing-api too.
- **pricing-api outage or slowness:** serve entries up to the hard TTL (20 minutes, longer than the last outage),
  flagged stale; cart views keep working. Payment fails closed with a clear retry message, because we cannot charge
  a stale price.
- **429 from pricing-api:** treat it like an outage for refreshes: back off exponentially with jitter, keep serving
  cached values, and alert if 429s persist for more than a minute.
- **Redis outage:** fall back to the per-instance cache plus direct batched calls with a local token bucket capped
  at 8 requests/s per instance (6 x 8 = 48 < 50).
- **Cold start after a deploy:** pre-warm the hot 2k SKUs from Redis on instance start.

## Alternatives considered

- **Per-instance cache only:** simplest, no Redis dependency, but each of the 6 instances refreshes independently,
  multiplying upstream traffic by 6 and giving users different prices across instances. Rejected as the primary
  layer; kept as the small hot tier.
- **Pricing pushes invalidations (events):** fresher data, but needs changes in pricing-api and an event pipeline
  we do not have, for data that changes at most every 5 minutes. Not worth it now.
- **Scheduled pre-warming of the whole hot set:** predictable upstream load, but refreshes SKUs nobody is viewing.
  We use it only at startup.

## Rollout and metrics

1. Ship behind a flag, read path in shadow mode: fetch from pricing-api as today, also read the cache, and log
   mismatches and staleness without using cached values.
2. Serve cached prices to 5%, then 25%, then 100% of cart views, a day at each step.
3. Metrics per step: cache hit rate (target > 95% memory+Redis), upstream request rate (target < 10 requests/s at
   peak), displayed-price staleness p99 (must stay < 5 minutes), price-mismatch rate at payment (< 0.5% of
   payments), cart-view p50 latency (target < 50 ms), and pricing 429s (target 0).
4. Roll back by turning the flag off; the cache is read-through, so no data migration is involved.
`),
		},
	},
};
