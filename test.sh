#!/bin/bash
set -euo pipefail

exec ./scripts/test-mempool.sh "$@"
