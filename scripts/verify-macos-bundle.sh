#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 /path/to/CheckPo.app" >&2
  exit 2
fi

bundle="$1"
if [[ ! -d "$bundle" || "$bundle" != *.app ]]; then
  echo "expected an existing .app bundle: $bundle" >&2
  exit 2
fi

if ! signature_details="$(codesign -dvv "$bundle" 2>&1)"; then
  echo "The bundle does not have a readable code signature." >&2
  exit 1
fi
if ! grep -q '^Authority=Developer ID Application:' <<<"$signature_details"; then
  echo "Developer ID signature is missing; ad-hoc signed bundles are not releasable." >&2
  exit 1
fi

codesign --verify --deep --strict --verbose=2 "$bundle"
spctl --assess --type execute --verbose=2 "$bundle"
xcrun stapler validate "$bundle"
