"""A cached value and when it expires."""


class Entry:
    __slots__ = ("value", "expires_at")

    def __init__(self, value, expires_at):
        self.value = value
        self.expires_at = expires_at

    def expired(self, now):
        return now > self.expires_at
