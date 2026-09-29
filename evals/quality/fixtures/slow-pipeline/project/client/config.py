"""Client settings for the staging order service.

TENANT is the id of a provisioned staging tenant and MAX_BATCH that tenant's batch limit (the most records one
upload request may carry). Both come from ./ops/provision.sh.
"""

TENANT = "unprovisioned"
MAX_BATCH = 10
