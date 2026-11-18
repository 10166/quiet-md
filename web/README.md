# Quiet.md for the web

The website's [example notebooks](../site/public/notebooks/) run Quiet.md itself, compiled for wasm32. The web and desktop apps share their source. GPUI's single-threaded web backend in `vendor/zed/crates/gpui_web` builds on the stable toolchain.

## Platform differences

- **Files.** The web library lives in memory (`fdl_core::vfs`). Desktop delegates to `std::fs`. The browser unpacks a library bundle on startup. Edits persist within the tab until refresh. Query paths through `PathExt` (e.g. `p.fs_exists()`), since native `Path` methods query the real filesystem. Use `fs_is_absolute()` because `Path::is_absolute()` always returns false on wasm32-unknown-unknown.
- **Time and processes.** Use `web_time::{Instant, SystemTime}`, which maps to std on desktop. In a browser, `std::time::Instant::now()` panics. Similarly, use `vfs::temp_dir()` and `vfs::process_id()` in place of `std::env::temp_dir()` and `std::process::id()`.
- **Fonts.** The browser cannot enumerate system fonts. The page downloads Inter and JetBrains Mono (OFL) from `fonts/` and uses browser fallback for Chinese (`CanvasFontFallback::EmojiAndCjk`). Vendor patch 0029 maps `.SystemUIFont` to Inter.
- **Images.** The bundle excludes `.assets/`. Images sit alongside it and download on first display via `vfs::add_lazy`, with a loading indicator while pending.
- **Shortcuts.** Mac browsers use macOS bindings (⌘), and other platforms use PC bindings (`shortcuts::mac()`).
- Global hotkeys, automatic updates, printing, system notifications, Quick Look and Show in Finder are desktop-only. The web entry point is `src/web.rs`.

## Build

Install the target with `rustup target add wasm32-unknown-unknown`, the wasm-bindgen-cli version matching Cargo.lock with `cargo install wasm-bindgen-cli --version <version> --root ~/.local/wasm-bindgen-<version>`, and Binaryen for `wasm-opt`.

```sh
web/build.sh site/public/demo          # Production: web profile and wasm-opt -Oz (a few minutes)
FAST=1 web/build.sh <directory>        # Debugging: release profile, skip wasm-opt
```

The generated `site/public/demo/` directory stays outside version control and deploys with the site (`cd site && npx wrangler@4 deploy`). Compressed transfers are about 6 MB of Wasm and 2 MB of fonts.

## Example libraries

Persona materials live in the private `marketing/mock-users/<name>/` directory. For each library:

1. Convert images to 1400 px JPEGs with `sips`, then import with the persona's `build_library.sh` using `FLAT=1`.
2. Open once with a devtools build to create `.quiet/index.json`. Set sidebar order with `web/curate.py <library> "<top-level-name>" …`.
3. Disable `auto_update` and `quick_hotkey` in preferences.
4. Run `web/bundle.py <library> site/public/demo/libraries/<name>.qmdb`. It also writes `libraries/<name>/.assets/`.

Open a library at `/demo/?lib=<name>`. To add a persona, generate the bundle, add an `index.html` modeled on an existing notebook and a 1280×800 devtools screenshot named `home.webp` in `site/public/notebooks/<name>/`. Add the entry to `notebooks/data.js` and a `section.notebook` to the homepage following Maya's example.

## Website

- `site/public/index.html` opens with the title, introduction and downloads. It then introduces the live app and shows Han Meimei's notebook followed by Maya's, each with a description and window. Each persona's own page uses the same layout for one notebook.
- Window styling follows measurements from a macOS 26 screenshot: continuous corners with a 23 pt curve (visually about a 16 pt radius), 14 pt traffic lights spaced 23 pt apart, with the first center at (26.75, 23.75). The webpage draws the controls. The web app reserves 92 pt as on macOS (`theme::TRAFFIC_LIGHTS_W`).
- On desktop, a window loads the app at `/demo/?lib=<name>` when it enters the viewport. Only one instance runs at a time. Starting another releases the offscreen one, which restarts on return. A transparent overlay lets the wheel scroll the page. Clicking removes the overlay and forwards the click to the app. Moving out restores it. Mobile shows `home.webp`.
- `site/public/window-resize.js` supplies resize behavior and styles. White outer handles resize the centered window horizontally on either side, vertically at the bottom, and in both dimensions at its lower corners. Double-click a handle to reset. Minimum size is 560×420. On first appearance the window briefly narrows and springs back, with a hint below. Resizing once sets `demo-resized` in localStorage to suppress the hint. These controls are desktop-only.
- The top GitHub button initially displays 1.9k stars, then fetches the live count from GitHub and caches it in the browser for one hour.
- The footer has another group of download buttons (`section.cta`).
- Persona descriptions in five languages live in `site/public/notebooks/data.js`.
