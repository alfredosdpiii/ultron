"""The (simulated) staging order service the load test uploads to."""


class Rejected(Exception):
    """The service refused a request."""


class StagingService:
    def __init__(self, tenant, max_batch):
        self.tenant = tenant
        self.max_batch = max_batch
        self.received = []
        self.requests = 0

    def post_batch(self, tenant, batch):
        """Accept one upload request: a non-empty list of at most `max_batch` records for this tenant."""
        self.requests += 1
        if tenant != self.tenant:
            raise Rejected(f"unknown tenant {tenant!r}")
        if not isinstance(batch, list) or not batch:
            raise Rejected("a batch must be a non-empty list")
        if len(batch) > self.max_batch:
            raise Rejected(f"batch of {len(batch)} records is over the tenant's limit of {self.max_batch}")
        self.received.extend(batch)
