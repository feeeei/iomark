#!/usr/bin/env bash
# Write <dir>/SHA256SUMS for the release assets and print the release body
# to stdout.
#
#   .github/release-notes.sh <artifact-dir> <tag>
set -euo pipefail

dir=${1:?usage: release-notes.sh <artifact-dir> <tag>}
tag=${2:?usage: release-notes.sh <artifact-dir> <tag>}

cd "$dir"
# Write outside the directory first: `sha256sum iomark-*` must not race with
# the creation of its own output file.
sha256sum iomark-* >../SHA256SUMS
mv ../SHA256SUMS .

platform_of() {
  case $1 in
  *x86_64-unknown-linux-gnu*) echo 'Linux x86_64' ;;
  *aarch64-unknown-linux-gnu*) echo 'Linux ARM64' ;;
  *macos-universal*) echo 'macOS (Apple Silicon + Intel)' ;;
  *x86_64-pc-windows-gnu*) echo 'Windows x86_64' ;;
  *aarch64-pc-windows-gnullvm*) echo 'Windows ARM64 (experimental)' ;;
  *) echo '-' ;;
  esac
}

base="https://github.com/${GITHUB_REPOSITORY:-feeeei/iomark}/releases/download/$tag"

cat <<EOF
## Install

macOS / Linux — install to \`~/.local/bin\`:

\`\`\`sh
curl -fsSL https://iomark.dev | sh
\`\`\`

Windows (PowerShell):

\`\`\`powershell
irm https://iomark.dev/install.ps1 | iex
\`\`\`

Benchmark right away without installing anything:

\`\`\`sh
curl -fsSL https://iomark.dev | sh -s -- quick
\`\`\`

Pin this exact release by adding \`-s -- install --version $tag\`.

## Downloads

| File | Platform | SHA-256 |
|---|---|---|
EOF

while read -r hash file; do
  printf '| [%s](%s/%s) | %s | `%s` |\n' "$file" "$base" "$file" "$(platform_of "$file")" "$hash"
done <SHA256SUMS

cat <<EOF

Manual download, verify and run (Linux x86_64 shown):

\`\`\`sh
curl -fsSLO $base/iomark-x86_64-unknown-linux-gnu.tar.gz
curl -fsSLO $base/SHA256SUMS
sha256sum --ignore-missing -c SHA256SUMS
tar xzf iomark-x86_64-unknown-linux-gnu.tar.gz
./iomark
\`\`\`

Every archive holds a single static \`iomark\` binary with fio embedded — no
runtime dependencies, nothing else to install. On macOS the build is a
universal binary covering both Apple Silicon and Intel.

EOF
