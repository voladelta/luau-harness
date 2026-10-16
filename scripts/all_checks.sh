#!/usr/bin/env bash
# All offline suites, with bounded runtime and per-suite timings.
cd "$(dirname "$0")/.." || exit 1
exec python3 scripts/all_checks.py "$@"
