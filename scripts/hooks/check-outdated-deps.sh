#!/usr/bin/env bash
# Outdated-dependency gate shared by the pre-commit and pre-push hooks.
#
# Detects outdated cargo dependencies and, if any are found, prompts the
# user to either abort and audit them (run /deps-audit in a Claude Code
# session — it reviews each version's source diff for backdoors before
# updating) or continue anyway.
#
# Skip with: MEWXI_SKIP_DEPS_CHECK=1 git commit ...
set -u

repo_root="$(git rev-parse --show-toplevel)" || exit 0
cd "$repo_root" || exit 0

if [ "${MEWXI_SKIP_DEPS_CHECK:-0}" = "1" ]; then
  exit 0
fi

outdated=""
if cargo outdated --version >/dev/null 2>&1; then
  # cargo-outdated sees semver-incompatible (major) bumps too.
  report="$(cargo outdated --root-deps-only --exit-code 1 --color never 2>/dev/null)"
  status=$?
  if [ $status -eq 1 ]; then
    outdated="$report"
  elif [ $status -ne 0 ]; then
    echo "deps-check: 'cargo outdated' failed (offline?); skipping." >&2
    exit 0
  fi
else
  # Fallback: only sees semver-compatible bumps. Install cargo-outdated
  # (cargo install cargo-outdated) for the full picture.
  report="$(cargo update --dry-run --color never 2>&1)"
  if [ $? -ne 0 ]; then
    echo "deps-check: 'cargo update --dry-run' failed (offline?); skipping." >&2
    exit 0
  fi
  outdated="$(printf '%s\n' "$report" \
    | grep -E '^[[:space:]]*(Updating|Adding|Removing)[[:space:]]' \
    | grep -v 'index' || true)"
fi

if [ -z "$outdated" ]; then
  exit 0
fi

echo ""
echo "deps-check: outdated cargo dependencies detected:"
echo ""
printf '%s\n' "$outdated" | sed 's/^/    /'
echo ""
if ! cargo outdated --version >/dev/null 2>&1; then
  echo "  (compatible bumps only — install cargo-outdated to also see major bumps)"
  echo ""
fi
echo "  To audit and update them, run /deps-audit in a Claude Code session:"
echo "  it reviews each update's source diff for backdoors, then updates."
echo ""

if [ ! -t 0 ] && [ ! -e /dev/tty ]; then
  echo "deps-check: no TTY to prompt on; blocking. Re-run with MEWXI_SKIP_DEPS_CHECK=1 to override." >&2
  exit 1
fi

printf "  Continue anyway without auditing? [y/N] "
read -r answer 2>/dev/null < /dev/tty || answer=""
case "$answer" in
  y|Y|yes|YES) exit 0 ;;
  *)
    echo "deps-check: aborted. Run /deps-audit in Claude Code, then retry." >&2
    exit 1
    ;;
esac
