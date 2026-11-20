#!/bin/bash
# Sign release packages with minisign, generate update-<platform>.json manifests,
# and upload them to a GitHub Release on GITHUB_REPOSITORY (default: 10166/quiet-md).
# Pass one or more packages (macOS ZIPs, Windows installer, Linux AppImage):
#   scripts/publish-downloads.sh dist-release.XXXX/quiet-md-0.1.14-macos-arm64.zip dist/windows/unsigned/quiet-md-0.1.14-windows-x64-setup.exe
# Each package is signed with minisign (MINISIGN_KEY, default ~/.minisign/quiet-md.key)
# and verified against packaging/update-key.pub. If the key is encrypted, set
# MINISIGN_PASSWORD or store it in macOS Keychain:
#   security add-generic-password -U -s quiet-md-minisign -a "$USER" -w
# DRY_RUN=1 prints the signing and upload commands without modifying GitHub.
set -euo pipefail
cd "$(dirname "$0")/.."
[ $# -ge 1 ] || { sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 1; }
repo=${GITHUB_REPOSITORY:-10166/quiet-md}
key=${MINISIGN_KEY:-$HOME/.minisign/quiet-md.key}
pub=packaging/update-key.pub
expected=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
tag="v$expected"
host="https://github.com/$repo/releases/download/$tag"
work=$(mktemp -d); trap 'rm -rf "$work"' EXIT

if [ -z "${DRY_RUN:-}" ]; then
  command -v minisign >/dev/null || { echo "minisign is not installed (brew install minisign)" >&2; exit 1; }
  command -v gh >/dev/null || { echo "gh is not installed (brew install gh)" >&2; exit 1; }
  grep -q '^RW' "$pub" || { echo "$pub has no public key yet" >&2; exit 1; }
  [ -f "$key" ] || { echo "Missing signing key $key (set MINISIGN_KEY)" >&2; exit 1; }
  password=${MINISIGN_PASSWORD:-}
  if [ -z "$password" ] && command -v security >/dev/null 2>&1; then
    password=$(security find-generic-password -s quiet-md-minisign -a "$USER" -w 2>/dev/null || true)
  fi
fi

assets=()
manifests=()

for file in "$@"; do
  name=$(basename "$file")
  case "$name" in
    quiet-md-*-macos-arm64.zip) platform=macos-arm64 ;;
    quiet-md-*-macos-x86_64.zip) platform=macos-x86_64 ;;
    quiet-md-*-windows-x64-setup.exe) platform=windows-x64 ;;
    quiet-md-*-x86_64.AppImage) platform=linux-x86_64 ;;
    *) echo "Unrecognised package name: $name" >&2; exit 1 ;;
  esac
  version=${name#quiet-md-}; version=${version%%-*}
  [ "$version" = "$expected" ] || { echo "$name is $version but Cargo.toml says $expected" >&2; exit 1; }
  [ -f "$file" ] || { echo "Missing $file" >&2; exit 1; }
  # Refuse a package that does not match the checksum written next to it by the build scripts.
  if [ -f "$file.sha256" ]; then
    if command -v shasum >/dev/null 2>&1; then
      (cd "$(dirname "$file")" && shasum -a 256 -c "$name.sha256" >/dev/null) || { echo "Checksum mismatch: $file" >&2; exit 1; }
    else
      (cd "$(dirname "$file")" && sha256sum -c "$name.sha256" >/dev/null) || { echo "Checksum mismatch: $file" >&2; exit 1; }
    fi
  fi
  # The trusted comment is the file name: the app accepts the signature only for the version the manifest names.
  sig="$work/$name.minisig"
  if [ -z "${DRY_RUN:-}" ]; then
    if [ -n "$password" ]; then
      printf '%s\n' "$password" | minisign -S -s "$key" -m "$file" -x "$sig" -t "$name" >/dev/null
    else
      minisign -S -s "$key" -m "$file" -x "$sig" -t "$name" </dev/null >/dev/null
    fi
    minisign -V -q -p "$pub" -m "$file" -x "$sig" || { echo "$name: signature does not match $pub" >&2; exit 1; }
  else
    echo "minisign -S -s $key -m $file -x $sig -t $name"
    : > "$sig"
  fi
  size=$(python3 -c 'import os, sys; print(os.path.getsize(sys.argv[1]))' "$file")
  manifest="$work/update-$platform.json"
  python3 - "$version" "$host/$name" "$size" "$sig" > "$manifest" <<'PYTHON'
import json, sys
version, url, size, sig = sys.argv[1:]
print(json.dumps({"version": version, "url": url, "size": int(size), "signature": open(sig).read()}, indent=2))
PYTHON
  assets+=("$file")
  [ -f "$file.sha256" ] && assets+=("$file.sha256")
  assets+=("$sig")
  manifests+=("$manifest")
  echo "Prepared $name ($size bytes) -> update-$platform.json ($version)"
done

if [ -n "${DRY_RUN:-}" ]; then
  printf 'gh release upload %q --repo %q --clobber' "$tag" "$repo"
  printf ' %q' "${assets[@]}" "${manifests[@]}"
  echo
  exit 0
fi

if ! gh release view "$tag" --repo "$repo" >/dev/null 2>&1; then
  gh release create "$tag" --repo "$repo" --title "Quiet.md $tag" --generate-notes
fi

# Upload packages first, then update-*.json manifests last so running copies
# only see the new version once the packages they point at are downloadable.
gh release upload "$tag" --repo "$repo" --clobber "${assets[@]}"
gh release upload "$tag" --repo "$repo" --clobber "${manifests[@]}"
echo "Published $tag to https://github.com/$repo/releases/tag/$tag"
