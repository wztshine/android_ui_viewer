# UI Viewer

A Rust desktop application for inspecting Android UI layouts from uiautomator XML dumps.
Built with [egui](https://github.com/emilk/egui) (immediate-mode GUI).
Single file (~1460 lines) located at `src/main.rs`.

## Features

- **Screenshot display** — load a device screenshot and hover/click to inspect elements
- **Element highlighting** — red outline on hover, green outline on selection
- **Collapsible tree view** — browse the UI hierarchy with expand/collapse arrows, click labels to select
- **Property inspection** — view class, text, resource-id, content-desc, and all XML attributes
- **ADB capture** — one-click to take screenshot + uiautomator dump from a connected Android device
- **uiautomator2 capture** — capture via atx-agent JSON-RPC (screenshot + hierarchy dump)
- **Double-click tap** — double-click on image to send `input tap` to device and auto-refresh
- **Drag swipe** — drag on image to send `input swipe` to device and auto-refresh (10px threshold)
- **Multi-display** — automatic detection of physical/logical display IDs, display selector in toolbar
- **Manual load** — load existing screenshot + XML pair from filesystem (auto-pairs files in same directory)
- **Save-as** — export current screenshot + XML pair via file dialog
- **Cross-platform** — works on Linux, Windows, and macOS

## Build & Run

```sh
# Debug build (fast compilation)
cargo build

# Release build (optimized)
cargo build --release
./target/release/uiviewer

# Build and run directly
cargo run
```

## Usage

1. **Load files** — click `Load Screenshot` and `Load XML` to select matching
   screenshot and dump files (auto-loads paired files from same directory)
2. **ADB Capture** — connect a device via USB, select it from the device dropdown,
   click `ADB` to pull live data
3. **U2 Capture** — connect a device with atx-agent installed, click `U2`
4. **Multi-display** — select display ID from the dropdown in toolbar
5. **Inspect** — hover over the screenshot to highlight elements; click to select
   (clicking the same spot cycles up through ancestors)
6. **Interact** — double-click to send tap command; drag to send swipe command
7. **Browse tree** — click arrows to expand/collapse; click labels to select;
   auto-scrolls to the selected node
8. **Properties** — view selected element's attributes in the right panel
   (drag the divider to adjust panel width)
9. **Save** — click `Save` to export the current screenshot + XML pair

The app also works completely offline with pre-captured files — no device needed
for browsing and inspection.

## Dependencies

- [eframe/egui](https://github.com/emilk/egui) — GUI framework
- [roxmltree](https://github.com/RazrFalcon/roxmltree) — XML parsing
- [image](https://github.com/image-rs/image) — image loading (PNG, JPEG)
- [rfd](https://github.com/PolyMeilex/rfd) — native file dialogs
- [serde_json](https://github.com/serde-rs/json) — JSON-RPC response parsing (uiautomator2)

## Requirements

- Rust edition 2021 or later
- ADB installed and in PATH (for device interaction)
- OpenGL 3.2+ (or Vulkan/DirectX 12 via wgpu) for rendering
- A connected Android device with USB debugging enabled (for ADB Capture / U2 Capture)
- **For U2 Capture**: [atx-agent](https://github.com/openatx/atx-agent) must be running on the device.
  Easiest: `python -m uiautomator2 init` (pushes atx-agent + app-uiautomator.apk).
  Manual: push `atx-agent` to `/data/local/tmp/atx-agent` and
  `app-uiautomator.apk` to `/data/local/tmp/app-uiautomator.apk` on the device.

> **Note**: Multi-display support is implemented but has not been tested on actual
> multi-display devices. Physical display ID mapping (`dumpsys SurfaceFlinger --display-id`),
> `--windows` XML parsing, and display selector logic are all code-complete but
> functionally unverified.

---

This repository's code was entirely written by **DeepSeek V4 Flash** in about 8 hours.
If the UI tree parsing has issues or other bugs occur, provide the UI tree file and
bug description to an AI to fix it.
