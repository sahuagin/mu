#!/usr/bin/env bash
# puppet-slots-test.sh — offline tests for the puppet slot provisioning script
# (crates/mu-irc-gateway/scripts/puppet-slots.py): what it makes of a server's
# answers, over a scripted socket. No server, no network, no model spend;
# `openssl` for the certificate cases (skipped without it).
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT/crates/mu-irc-gateway/scripts"
python3 -m unittest -v test_puppet_slots
