#!/bin/sh
# Slow on purpose: see README.md. Do not edit anything under ops/.
cd "$(dirname "$0")/.." && exec python3 -B ops/lib/loadtest.py "$@"
