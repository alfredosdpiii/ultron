# delta: layered configuration

Builds a service's configuration from three layers: built-in defaults, a config file (already parsed into a dict)
and environment variables. Later layers win.

## deep_merge(base, override)

Returns a new dict:

- Keys only in `base` are kept; keys only in `override` are added.
- When both values are dicts, they are merged recursively by these same rules.
- Otherwise the override value replaces the base value (lists are replaced, never concatenated).
- An override value of `None` deletes the key from the result (at any depth). A `None` inside an override dict
  that has nothing to merge into is dropped as well, so the result never contains `None` values from `override`.
- Neither input is ever modified, at any depth. Calling `deep_merge` (or `load`) again with the same inputs gives
  the same result.

## env_overrides(environ, prefix="APP__")

Turns environment variables into an override dict:

- Only variables whose name starts with `prefix` are used. The rest of the name is split on double underscores
  (`__`) into a key path; each segment is lowercased. Single underscores are part of a key name:
  `APP__DB__MAX_CONNECTIONS=20` sets `{"db": {"max_connections": 20}}`.
- A name with an empty segment (such as `APP__DB____PORT` or `APP__`) is ignored.
- Variables are applied in sorted order of their names; a later path that runs through a non-dict value replaces
  that value with a dict.
- Values are converted by `coerce`.

## coerce(text)

Surrounding whitespace is ignored when recognising the typed values below:

- `true` / `false` in any case: `True` / `False`.
- `null` in any case: `None` (so the variable deletes the key).
- An optionally negative integer (`42`, `-7`): `int`.
- An optionally negative decimal with digits on both sides of the point (`0.5`, `-2.25`): `float`.
- Anything else is a string, returned exactly as given (whitespace included).

## load(defaults, file_config=None, environ=None)

`deep_merge(deep_merge(defaults, file_config or {}), env_overrides(environ or {}))`.
