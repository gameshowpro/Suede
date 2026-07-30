#!/usr/bin/env bash
#
# Generate the documentation site's derived assets.
#
#   1. The OpenAPI document, produced by the binary being released, so the
#      published reference can never drift from the code.
#   2. The Scalar bundle, vendored locally so the published page makes no
#      requests to a CDN at view time.
#
# Everything it writes lands in docs/generated/, which is git-ignored.
set -euo pipefail

cd "$(dirname "$0")/.."
OUT="docs/generated"
SCALAR_VERSION="${SCALAR_VERSION:-1.25.0}"
SCALAR_URL="https://cdn.jsdelivr.net/npm/@scalar/api-reference@${SCALAR_VERSION}/dist/browser/standalone.js"

mkdir -p "$OUT"

echo "==> Generating the OpenAPI document"
if [[ -x "${SUEDE_BIN:-}" ]]; then
  "$SUEDE_BIN" openapi > "$OUT/openapi.json"
else
  cargo run --quiet -- openapi > "$OUT/openapi.json"
fi
python3 - "$OUT/openapi.json" <<'PY'
import json, sys
document = json.load(open(sys.argv[1]))
print(f"    {len(document['paths'])} paths, "
      f"{len(document['components']['schemas'])} schemas")
PY

echo "==> Vendoring the Scalar bundle (${SCALAR_VERSION})"
if [[ -f "$OUT/scalar.js" && -n "${SKIP_SCALAR_DOWNLOAD:-}" ]]; then
  echo "    reusing the existing bundle"
else
  curl -fsSL "$SCALAR_URL" -o "$OUT/scalar.js"
  echo "    $(wc -c < "$OUT/scalar.js") bytes"
fi

echo "==> Done"
