#!/bin/sh
# Assert that a built binary reports the embedded fio version, and — on a
# release build — the tag being released.
#
#   .github/check-version.sh <path-to-iomark>
set -eu

bin=${1:?usage: check-version.sh <path-to-iomark>}
out=$("$bin" --version)
printf '%s\n' "$out"

case $out in
*"(fio-"*) ;;
*)
  printf 'error: no embedded fio version in %s\n' "$out" >&2
  exit 1
  ;;
esac

if [ -n "${IOMARK_RELEASE_TAG:-}" ]; then
  expected="iomark ${IOMARK_RELEASE_TAG#v}"
  case $out in
  "$expected "*) ;;
  *)
    printf 'error: expected "%s (fio-...)", got "%s"\n' "$expected" "$out" >&2
    exit 1
    ;;
  esac
fi
