"""Drop repeated records."""


def dedupe(records, key):
    """Keep one record per non-empty `key` value, in input order; records with an empty value are all kept."""
    chosen = {}
    for index, record in enumerate(records):
        value = record.get(key, "")
        chosen[value if value else ("blank", index)] = index
    return [records[index] for index in sorted(chosen.values())]
