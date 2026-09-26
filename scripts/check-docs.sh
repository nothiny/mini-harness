#!/usr/bin/env bash
#
# check-docs.sh — verify that tutorial documentation references still resolve.
#
# Scope: only docs/tutorial/** (the tutorial is the public documentation set).
#
# Checks performed:
#   1. `cargo test --test <target>`  → tests/<target>.rs must exist
#   2. `cargo test <filter>`         → the filter must match at least one test
#   3. referenced src/** / tests/**  → the path must exist
#   4. relative markdown links       → the target must exist
#
# Usage (from the repository root, after a build so tests can be listed):
#   bash scripts/check-docs.sh
#
# CI runs this right after `cargo test`, which warms the build cache.

set -euo pipefail

cd "$(dirname "$0")/.."

docs=()
while IFS= read -r f; do docs+=("$f"); done < <(find docs/tutorial -name '*.md' | sort)

fail=0
report() {
  echo "check-docs: $1" >&2
  fail=1
}

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

# --------------------------------------------------------------- cargo test
if ! cargo test -- --list >"$tmp" 2>/dev/null; then
  echo "check-docs: unable to list tests; run 'cargo build' (or 'cargo test') first" >&2
  exit 2
fi
test_names="$(sed -n 's/: test$//p' "$tmp")"

grep -rhoE 'cargo test[^`"]*' "${docs[@]}" 2>/dev/null \
  | sed -E 's/[[:space:]]*#.*$//' | sed -E 's/[[:space:]]+$//' | sort -u >"$tmp" || true
while IFS= read -r cmd; do
  [ -z "$cmd" ] && continue
  # shellcheck disable=SC2086
  set -- $cmd
  shift 2 || continue              # drop `cargo test`
  target=""
  filter=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --test) target="${2:-}"; shift 2 2>/dev/null || shift ;;
      --)     shift ;;
      -*)     shift ;;
      *)      [ -z "$filter" ] && filter="$1"; shift ;;
    esac
  done
  if [ -n "$target" ]; then
    [ -f "tests/$target.rs" ] || report "missing test target: cargo test --test $target"
  elif [ -n "$filter" ]; then
    if ! printf '%s\n' "$test_names" | grep -Fq -- "$filter"; then
      report "cargo test '$filter' matches no test"
    fi
  fi
done <"$tmp"

# ------------------------------------------------------------ source paths
# Split on non-path characters first, so `ui/src/foo.ts` is not read as `src/foo.ts`.
cat "${docs[@]}" 2>/dev/null | tr -c 'A-Za-z0-9_./-' '\n' \
  | grep -E '^(src|tests)/' | sort -u >"$tmp" || true
while IFS= read -r path; do
  [ -z "$path" ] && continue
  [ -e "$path" ] || report "missing source path: $path"
done <"$tmp"

# ----------------------------------------------------------- markdown links
for doc in "${docs[@]}"; do
  dir="$(dirname "$doc")"
  grep -oE '\]\([^)]+\)' "$doc" 2>/dev/null \
    | sed -E 's/^\]\(//; s/\)$//' | sort -u >"$tmp" || true
  while IFS= read -r link; do
    case "$link" in
      ''|'#'*|http://*|https://*|mailto:*) continue ;;
    esac
    target="${link%%#*}"
    [ -z "$target" ] && continue
    [ -e "$dir/$target" ] || report "broken link in $doc -> $link"
  done <"$tmp"
done

if [ "$fail" -eq 0 ]; then
  echo "check-docs: all documentation references resolve"
fi
exit "$fail"
