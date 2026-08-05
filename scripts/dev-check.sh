#!/usr/bin/env bash
# Local development check: build, lint, unit test, then smoke test.
#
# Mirrors what CI does, with one exception: `all` does not build the docs,
# because that needs pip and a network, which a working checkout may not have.
# Run `dev-check.sh docs` separately when documentation changed. CI builds it
# on every push either way, so the gap is caught - just later.
set -uo pipefail

export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/suede-target}"
cd "$(dirname "$0")/.." || exit 1

STAGE="${1:-all}"
FAILED=0

run_stage() {
  local name="$1"
  shift
  echo "===== $name ====="
  if "$@"; then
    echo "----- $name OK"
  else
    echo "----- $name FAILED"
    FAILED=1
  fi
  echo
}

if [[ "$STAGE" == "docs" ]]; then
  # Build the documentation site exactly as CI does.
  python3 -m pip install --quiet --user -r docs/requirements.txt || {
    echo "could not install mkdocs"; exit 1; }
  export PATH="$HOME/.local/bin:$PATH"
  cargo build --quiet || exit 1
  SUEDE_BIN="$CARGO_TARGET_DIR/debug/suede" SKIP_SCALAR_DOWNLOAD=1 bash scripts/build-docs.sh || exit 1
  SUEDE_DOCS_VERSION=dev mkdocs build --strict
  exit $?
fi

if [[ "$STAGE" == "snapshot" ]]; then
  # Regenerate the committed OpenAPI snapshot.
  UPDATE_SNAPSHOT=1 cargo test --test openapi_snapshot 2>&1 | tail -5
  exit 0
fi

if [[ "$STAGE" == "fix" ]]; then
  # Apply formatting and report remaining clippy findings in full.
  cargo fmt --all
  cargo clippy --all-targets 2>&1 | grep -vE '^\s*(Compiling|Checking|Finished)' | head -120
  exit 0
fi

if [[ "$STAGE" == "all" || "$STAGE" == "lint" ]]; then
  run_stage "cargo fmt" cargo fmt --all -- --check
  run_stage "cargo clippy" cargo clippy --all-targets -- -D warnings
fi

if [[ "$STAGE" == "all" || "$STAGE" == "test" ]]; then
  run_stage "cargo test" cargo test
fi

if [[ "$STAGE" == "all" || "$STAGE" == "smoke" ]]; then
  cargo build -q || FAILED=1
  BIN="$CARGO_TARGET_DIR/debug/suede" run_stage "smoke test" \
    env BIN="$CARGO_TARGET_DIR/debug/suede" bash scripts/smoke-test.sh
fi

echo "====================="
if [[ "$FAILED" -eq 0 ]]; then
  echo "ALL STAGES PASSED"
  # Naming what was not covered, so this banner cannot be read as more than
  # it is. Only when docs could actually have changed - otherwise it is noise
  # that teaches you to ignore the line.
  if [[ "$STAGE" == "all" ]] && ! git diff --quiet --exit-code HEAD -- docs mkdocs.yml 2>/dev/null; then
    echo "(docs not built - you changed them; run: ./scripts/dev-check.sh docs)"
  fi
else
  echo "SOME STAGES FAILED"
fi
exit "$FAILED"
