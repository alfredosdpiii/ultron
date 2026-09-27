"""Recursive dict merge where None deletes."""


def deep_merge(base, override):
    result = dict(base)
    for key, value in override.items():
        if value is None:
            result.pop(key, None)
        elif isinstance(value, dict):
            current = result.get(key)
            if isinstance(current, dict):
                _merge_into(current, value)
            else:
                result[key] = deep_merge({}, value)
        else:
            result[key] = value
    return result


def _merge_into(target, override):
    for key, value in override.items():
        if value is None:
            target.pop(key, None)
        elif isinstance(value, dict) and isinstance(target.get(key), dict):
            _merge_into(target[key], value)
        elif isinstance(value, dict):
            target[key] = deep_merge({}, value)
        else:
            target[key] = value
