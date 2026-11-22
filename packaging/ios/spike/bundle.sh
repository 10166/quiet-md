#!/bin/zsh
# Spike: package target/aarch64-apple-ios-sim/<profile>/quiet_md as a simulator .app.
set -e
cd "$(git rev-parse --show-toplevel)"
profile=${1:-debug}
app=target/ios-spike/$profile/Quiet.md.app
mkdir -p $app
cp target/aarch64-apple-ios-sim/$profile/quiet_md $app/quiet_md
cp packaging/ios/spike/app/Info.plist $app/
codesign --force --sign - $app
echo $PWD/$app
