# services

Six small, independent Python services (standard library only, Python 3.10+). They share no code.

| Service | Package | What it does |
| --- | --- | --- |
| services/alpha | ratelimit | per-key token-bucket rate limiter |
| services/beta | invoice | invoice totals with decimal rounding and discount allocation |
| services/gamma | windows | reporting date windows |
| services/delta | confmerge | layered configuration (defaults, file, environment) |
| services/epsilon | ttlcache | LRU cache with expiry |
| services/zeta | csvnorm | customer CSV normalization |

Each service's behaviour is specified in its SPEC.md. Quick tests, run from the service's directory:

    cd services/alpha && python3 -m unittest discover -s tests
