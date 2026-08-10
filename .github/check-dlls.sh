#!/bin/sh
# Assert that a Windows build imports only DLLs that ship with Windows.
#
# MSYS2 hands out zlib and libwinpthread as import libraries by default, and
# the DLLs behind them live in the MSYS2 prefix. A release binary importing
# one of those runs fine in CI — the MSYS2 shell has them on PATH — and starts
# nowhere else: the loader fails before main with STATUS_DLL_NOT_FOUND and
# kills the process without a word, which from the outside is indistinguishable
# from a program that ran and printed nothing.
#
#   .github/check-dlls.sh <path-to-iomark.exe>
set -eu

bin=${1:?usage: check-dlls.sh <path-to-iomark.exe>}

dump=
for tool in objdump llvm-objdump; do
  if command -v "$tool" >/dev/null 2>&1; then
    dump=$tool
    break
  fi
done
[ -n "$dump" ] || {
  printf 'error: neither objdump nor llvm-objdump is available\n' >&2
  exit 1
}

# Presence in System32 is the test, rather than a hand-kept allow list: it is
# exactly the question the loader will ask on the user's machine. The api-ms-win-*
# API sets are real files there too.
if command -v cygpath >/dev/null 2>&1; then
  system32=$(cygpath -u "${SYSTEMROOT:-C:\\Windows}")/System32
else
  system32=/c/Windows/System32
fi
[ -d "$system32" ] || {
  printf 'error: cannot locate System32 (looked in %s)\n' "$system32" >&2
  exit 1
}

dlls=$("$dump" -p "$bin" | sed -n 's/^[[:space:]]*DLL Name:[[:space:]]*//p' | sort -fu)
[ -n "$dlls" ] || {
  printf 'error: %s imports no DLL at all — is it a PE image?\n' "$bin" >&2
  exit 1
}

foreign=
for dll in $dlls; do
  # -iname, not a plain [ -e ]: an import table spells the name however the
  # import library did (KERNEL32.dll), System32 holds it lowercase, and only
  # NTFS's case-insensitivity would paper over the difference.
  if [ -n "$(find "$system32" -maxdepth 1 -iname "$dll" -print -quit 2>/dev/null)" ]; then
    printf '  ok  %s\n' "$dll"
  else
    printf '  BAD %s\n' "$dll"
    foreign="$foreign $dll"
  fi
done

[ -z "$foreign" ] || {
  printf 'error: %s imports DLLs that are not part of Windows:%s\n' "$bin" "$foreign" >&2
  printf 'link them statically in build.rs (see WINDOWS_STATIC_LIBS)\n' >&2
  exit 1
}
