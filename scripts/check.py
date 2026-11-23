#!/usr/bin/env python3
"""Shared macOS, Windows and Linux checks; run inside the platform build environment."""
import os
from pathlib import Path
import subprocess
import sys

os.chdir(Path(__file__).resolve().parent.parent)
def cargo(*args, env=None):
    subprocess.run(["cargo", *args], check=True, env=env)

def has_metal_compiler():
    sdk = os.environ.get("SDKROOT") or "macosx"
    try:
        return all(
            subprocess.run(
                ["xcrun", "-sdk", sdk, "-f", tool],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            ).returncode == 0
            for tool in ("metal", "metallib")
        )
    except OSError:
        return False

# Full Xcode supplies the offline Metal shader compiler; on CommandLineTools-only
# Macs, compile the same Metal shaders at runtime instead. CI stays strict.
runtime_shaders = sys.platform == "darwin" and not os.environ.get("CI") and not has_metal_compiler()
if runtime_shaders:
    print("Xcode metal compiler not found; using gpui_platform/runtime_shaders")
shader_args = ["--features", "gpui_platform/runtime_shaders"] if runtime_shaders else []
devtools_features = "devtools,gpui_platform/runtime_shaders" if runtime_shaders else "devtools"

cargo("test", "--release", "--workspace", "--locked", *shader_args)
for library in ["shared-mock/library", "shared-mock/library-zh"]:
    cargo("test", "--release", "--locked", "-p", "fdl_core", "library_round_trips", "--", "--ignored",
          env={**os.environ, "FDL_ROUNDTRIP_DIR": str(Path(library).resolve())})
source = Path("shared-mock/guides/assets")
bundled = Path("shared-mock/library/.assets/guides")
for image in source.rglob("*"):
    if image.is_file():
        target = bundled / image.relative_to(source)
        assert target.read_bytes() == image.read_bytes(), f"Bundled guide asset is stale: {target}"
print("Bundled guide assets match their sources")
cargo("clippy", "--release", "--workspace", "--locked", "--all-targets", *shader_args, "--", "-D", "warnings")
# The harness build (script driver compiled in) must stay clean too.
cargo("clippy", "--release", "--workspace", "--locked", "--all-targets", "--features", devtools_features, "--", "-D", "warnings")
# 数据表 is off by default (Cargo feature `datatable`); the build with it on must stay clean too.
cargo("clippy", "--release", "--workspace", "--locked", "--all-targets", "--features", f"{devtools_features},datatable", "--", "-D", "warnings")
