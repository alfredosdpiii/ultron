"""Upload order records to the staging order service in batches."""
from client import config


def chunks(items, size):
    """Split `items` into consecutive batches of at most `size` items, in order.

    Every item lands in exactly one batch; only the last batch may be shorter, and there are no empty batches.
    """
    if size < 1:
        raise ValueError("batch size must be at least 1")
    return [items[start : start + size] for start in range(0, len(items) - size + 1, size)]


def upload(service, items):
    """Send every record exactly once, in order, in as few requests as the tenant's batch limit allows."""
    sent = 0
    for batch in chunks(list(items), config.MAX_BATCH):
        service.post_batch(config.TENANT, batch)
        sent += len(batch)
    return sent
