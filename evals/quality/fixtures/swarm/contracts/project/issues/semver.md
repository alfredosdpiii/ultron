# semver: "1.10.0" sorts before "1.9.0"

Versions are compared as strings. Expected behaviour (semver.org 2.0.0 precedence):

- Major, minor and patch compare numerically: `compare("1.10.0", "1.9.0") == 1`.
- A pre-release sorts before its release: `compare("1.0.0-alpha", "1.0.0") == -1`.
- Pre-release identifiers compare left to right, dot separated: numeric identifiers numerically, others in ASCII
  order, numeric before non-numeric, and a shorter list first when all its identifiers are equal:
  `1.0.0-alpha < 1.0.0-alpha.1 < 1.0.0-alpha.beta < 1.0.0-beta < 1.0.0-beta.2 < 1.0.0-beta.11 < 1.0.0-rc.1 < 1.0.0`.
- Build metadata (after `+`) is ignored: `compare("1.0.0+build.5", "1.0.0+other") == 0`.
- A malformed version (`"1.0"`, `"1.0.x"`, `""`) raises `ValueError`.
