#!/bin/bash
# App Store Connect setup for the iPhone TestFlight beta (bundle ids, app
# record, test info, public group, beta review). Thin wrapper that supplies
# the Python deps; see scripts/ios/asc.py and docs/godspeed/TESTFLIGHT.md.
#
#   scripts/ios/asc-setup.sh status|caps|create-app|beta-info|group|submit|all
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
exec uv run --quiet --no-project --python 3.12 --with 'pyjwt[crypto]' --with requests \
  python "$ROOT/scripts/ios/asc.py" "$@"
