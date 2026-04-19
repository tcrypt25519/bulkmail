#!/bin/bash
# Canonical command for running manual P2P tests.
# This script should be updated whenever the required features or arguments change.

unset MEMPOOLORACLE_WS_URL
RUST_LOG=mempooloracle=info cargo run -p mempooloracle --example cli_tool --features reth-p2p,consensus-p2p -- --p2p --p2p-log-path /tmp/p2p_audit.log --metrics-port 9091 "$@"
