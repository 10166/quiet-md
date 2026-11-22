#!/bin/zsh
# Spike: build, bundle, install and launch on the booted iPad simulator; console to $LOG.
set -e
cd "$(git rev-parse --show-toplevel)"
dev=${DEV:-170556F7-4D34-46DF-AEB1-CF4A31883C35}
IPHONEOS_DEPLOYMENT_TARGET=17.0 cargo build --target aarch64-apple-ios-sim 2>&1 | grep -E '^(error|warning: unused)' -A5 || true
packaging/ios/spike/bundle.sh >/dev/null
xcrun simctl install $dev target/ios-spike/debug/Quiet.md.app
(SIMCTL_CHILD_QUIET_MD_HOME=$LIB SIMCTL_CHILD_RUST_BACKTRACE=1 xcrun simctl launch --console-pty --terminate-running-process $dev com.s1dashu.quietmd.spike > $LOG 2>&1 &)
