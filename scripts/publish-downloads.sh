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
# --policy instead of packages signs and uploads policy.json, the forced update (docs/UPDATES.md), to the
# release of the version in Cargo.toml, where the app reads it (releases/latest/download/policy.json):
#   MIN_VERSION=0.1.15 MESSAGE_ZH='…' MESSAGE_EN='…' scripts/publish-downloads.sh --policy
#   PLATFORM_MIN='windows-x64=0.1.16' (per platform, in place of MIN_VERSION); neither set clears the policy.
# Publishing packages takes the policy along from the newest release, so it stays in force. Downloadable
# fonts are not published here: copies fetch them from download.quiet.md/fonts (src/font_downloads.rs).
set -euo pipefail
export PYTHONIOENCODING=utf-8
cd "$(dirname "$0")/.."
[ $# -ge 1 ] || { sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'; exit 1; }
repo=${GITHUB_REPOSITORY:-10166/quiet-md}
key=${MINISIGN_KEY:-$HOME/.minisign/quiet-md.key}
pub=packaging/update-key.pub
expected=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
tag="v$expected"
host="https://github.com/$repo/releases/download/$tag"
latest="https://github.com/$repo/releases/latest/download"
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

sign() {  # file trusted-comment signature
  if [ -n "${password:-}" ]; then printf '%s\n' "$password" | minisign -S -s "$key" -m "$1" -x "$3" -t "$2" >/dev/null
  else minisign -S -s "$key" -m "$1" -x "$3" -t "$2"; fi
  minisign -V -q -p "$pub" -m "$1" -x "$3" || { echo "$(basename "$1"): signature does not match $pub" >&2; exit 1; }
}

# The policy text is signed as is; its trusted comment names it by SHA-256, so the signature fits no other
# text and is never mistaken for a package's. Refused when a platform's newest package is older than required.
if [ "$1" = --policy ]; then
  [ $# -eq 1 ] || { echo "--policy takes no packages" >&2; exit 1; }
  body="$work/policy.txt"
  python3 - "${MIN_VERSION:-}" "${PLATFORM_MIN:-}" "${MESSAGE_ZH:-}" "${MESSAGE_EN:-}" > "$body" <<'PYTHON'
import json, re, sys, datetime
min_version, platform_min, zh, en = sys.argv[1:]
version = re.compile(r"^\d+(\.\d+)*$")
platforms = dict(p.split("=", 1) for p in platform_min.split())
for v in [min_version, *platforms.values()]:
    if v and not version.match(v): sys.exit(f"Not a version: {v}")
for p in platforms:
    if p not in ("macos-arm64", "macos-x86_64", "windows-x64", "linux-x86_64"): sys.exit(f"Unknown platform: {p}")
policy = {"min_version": min_version, "platforms": platforms, "message_zh": zh, "message_en": en,
          "issued": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")}
sys.stdout.write(json.dumps(policy, ensure_ascii=False, separators=(",", ":"), sort_keys=True))
PYTHON
  echo "Policy: $(cat "$body")"
  [ -z "${MIN_VERSION:-}${PLATFORM_MIN:-}" ] && echo "(no version required: this clears the forced update)"
  newer() { python3 -c 'import sys; p=lambda v:[int(x) for x in v.split(".")]; a,b=p(sys.argv[1]),p(sys.argv[2]); n=max(len(a),len(b)); sys.exit(0 if a+[0]*(n-len(a))>b+[0]*(n-len(b)) else 1)' "$1" "$2"; }
  for platform in macos-arm64 macos-x86_64 windows-x64 linux-x86_64; do
    need=$(sed -n "s/.*$platform=\([0-9.]*\).*/\1/p" <<<"${PLATFORM_MIN:-}"); need=${need:-${MIN_VERSION:-}}
    [ -n "$need" ] || continue
    have=$(curl -sfL "$latest/update-$platform.json" | python3 -c 'import json,sys; print(json.load(sys.stdin)["version"])' || true)
    [ -n "$have" ] || { echo "Could not read the newest release's update-$platform.json" >&2; [ -n "${DRY_RUN:-}" ] && continue; exit 1; }
    if newer "$need" "$have"; then echo "$platform: latest is $have, older than the required $need; publish the packages first" >&2; exit 1; fi
    echo "$platform: copies before $need update to $have"
  done
  comment="quiet-md-policy sha256:$(shasum -a 256 "$body" | cut -d' ' -f1)"
  if [ -z "${DRY_RUN:-}" ]; then sign "$body" "$comment" "$body.minisig"
  else echo "minisign -S -s $key -m $body -x $body.minisig -t '$comment'"; : > "$body.minisig"; fi
  python3 - "$body" "$body.minisig" > "$work/policy.json" <<'PYTHON'
import json, sys
print(json.dumps({"policy": open(sys.argv[1], encoding="utf-8").read(), "signature": open(sys.argv[2]).read()}, ensure_ascii=False, indent=2))
PYTHON
  if [ -n "${DRY_RUN:-}" ]; then echo "gh release upload $tag --repo $repo --clobber $work/policy.json"; exit 0; fi
  gh release upload "$tag" --repo "$repo" --clobber "$work/policy.json"
  echo "$latest/policy.json updated"
  exit 0
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
    sign "$file" "$name" "$sig"
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

# A policy in force stays so across releases: the newest release's goes along to this one.
if gh release download --repo "$repo" --pattern policy.json --dir "$work/policy" 2>/dev/null; then assets+=("$work/policy/policy.json"); fi

if ! gh release view "$tag" --repo "$repo" >/dev/null 2>&1; then
  gh release create "$tag" --repo "$repo" --title "Quiet.md $tag" --generate-notes
fi

# Upload packages first, then update-*.json manifests last so running copies
# only see the new version once the packages they point at are downloadable.
gh release upload "$tag" --repo "$repo" --clobber "${assets[@]}"
gh release upload "$tag" --repo "$repo" --clobber "${manifests[@]}"
echo "Published $tag to https://github.com/$repo/releases/tag/$tag"
