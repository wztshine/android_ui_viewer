# uiviewer — Android UI Inspection Tool

## Description

Rust GUI tool that lets you inspect Android UI layouts from uiautomator XML dumps.
Displays a screenshot with hover/click element highlighting, a collapsible tree view of
the UI hierarchy, and attribute inspection. Supports ADB capture (screencap + dump),
uiautomator2 (JSON-RPC) capture, manual file loading, and save-as.

## Tech Stack

- **Language**: Rust
- **GUI**: eframe/egui 0.27
- **XML parsing**: roxmltree 0.19
- **Image loading**: image 0.25 (png, jpeg)
- **File dialogs**: rfd 0.14
- **JSON**: serde_json 1.0
- **ADB**: via `std::process::Command`
- **Rendering**: glow backend (OpenGL)

## Project Layout

```
/var/my_share/projects/uiviewer/
├── src/main.rs      # single-file app (~1460 lines)
├── Cargo.toml
├── AGENTS.md
└── README.md
```

## Architecture

### Core Types

- **`UiNode`** — tree node from XML: `bounds`, `attrs`, `children`
  - `find_branch(pos, path)` — finds the smallest-area node at image pixel (walks children first, then checks own bounds; handles child bounds exceeding parent)
  - `node_at(path)` — follows `Vec<usize>` path to a node
- **`App`** — egui app state with screenshot/texture, paths, expanded set, selection/hover state
- **`CaptureMethod`** — `Adb` | `U2`

### Key Functions

| Function | Role |
|---|---|
| `parse_xml(xml)` | Parse standard uiautomator dump into `UiNode` tree; handles multiple top-level `<node>` via `merge_nodes` |
| `parse_windows_xml(text, display_id)` | Parse `--windows` format (`<displays>` / hybrid / direct `<display>` children), extract nodes for given display |
| `get_display_ids_from_xml(text)` | Detect multi-display format and return list of display IDs from XML |
| `merge_nodes(nodes)` | Wrap multiple root nodes in a synthetic `FrameLayout` root |
| `load_texture(path, ctx)` | Load PNG/JPEG into egui texture |
| `get_displays(serial)` | Query device for `Vec<(logical_id, physical_id)>` via `dumpsys display` + `dumpsys SurfaceFlinger --display-id` |
| `adb_capture(serial, display_logical, display_physical, use_windows)` | ADB capture: `screencap -p [-d <phys>]` + `uiautomator dump [--windows]` + `adb pull` |
| `uiautomator2_capture(serial, display_id)` | U2 capture: atx-agent lifecycle, `/screenshot/{id}` + `/dump/hierarchy` via raw TCP |
| `http_get(path)` | Send raw HTTP GET to atx-agent (U2 JSON-RPC) |
| `render_tree(ui, node, ...)` | Recursively render collapsible tree with arrows, indentation, colored labels |
| `tree_label(node)` | Format node label: `ClassName "text" [resource-id]` |

### Layout Structure

```
CentralPanel                              — Manual left/right split with draggable 4px divider
  ├── left side                           — Screenshot image with hover/click/drag overlays
  ├── divider (draggable)                 — Updates `properties_width` field
  └── right side                          — Node Tree + Properties panels
TopBottomPanel::top("toolbar")            — Load/ADB/U2/Save buttons + display selector + file names
TopBottomPanel::bottom("status")          — Status messages
```

### Tree Click Handling (critical pattern)

Tree items use a **plain `Label` (no `Sense`)** to avoid ScrollArea auto-scrolling.
Click detection is done entirely via raw input:

1. Each node's screen-space `rect` is recorded in `node_rects: Vec<(Vec<usize>, Rect)>`
2. After tree renders, `ui.input(|i| i.pointer.any_click())` checks for clicks
3. `interact_pos()` gives the click position; matched against `node_rects`
4. Arrow expand/collapse uses `Label::sense(Sense::click())` and does NOT cause jumps

This avoids `selectable_label` and any interactive widget inside ScrollArea that would
trigger auto-scroll-to-focused-widget behavior.

### Image Interaction

- **Click**: `response.clicked()` → `selected_path` + `scroll_to_target` → green highlight + tree scroll + ancestor auto-expand. Same-spot click cycles up through ancestors.
- **Double-click**: `response.double_clicked()` → `pending_tap` → 800ms settle → re-capture. Sends `input tap [--display <id>]` to device.
- **Drag**: `Sense::click_and_drag()` → `drag_start_img` → on release with distance ≥ 10px, sends `input swipe [--display <id>] <x1> <y1> <x2> <y2>` → 800ms settle → re-capture.
- **Hover**: `response.hover_pos()` → `find_branch()` → `hovered_path` → red highlight + property preview. Skipped if mouse hasn't moved ≥ 5px (`last_hover_img_pos`).

### Multi-Display Support

- **Display detection**: `get_displays(serial)` returns `Vec<(u32, u64)>` pairing logical IDs (from `dumpsys display`) with physical IDs (from `dumpsys SurfaceFlinger --display-id`)
- **Display selector**: ComboBox in toolbar (always when device connected) and Node Tree panel (when file loaded)
- **ADB**: multi-display uses `screencap -d <phys_id>` + `uiautomator dump --windows`; single uses `screencap -p` + standard `uiautomator dump`
- **U2**: screenshot via `/screenshot/{display_id}`; hierarchy via `/dump/hierarchy` (no display param, atx-agent may return all displays)
- **File parsing**: `parse_windows_xml` handles `<hierarchy><displays><display>`, hybrid `<hierarchy><display>`, and `--windows` `<displays><display><window><hierarchy>` formats
- **On display change**: for file-loaded data, re-parses XML from `file_xml_content` with new `display_id`; for device capture, re-captures with new display

### Properties Panel

- Manual split inside `CentralPanel` (not `SidePanel`) — prevents content width from influencing panel width
- `properties_width: f32` field, draggable 4px divider, clamped 80..70% of panel
- Labels on separate lines (key bold, value selectable+wrap)

### find_branch Behavior

- **Smallest area** selection: prefers most specific (innermost) element at click point
- **Ancestor-boundary tolerance**: checks children regardless of whether current node's bounds contain the point; handles cases like WebView/ComposeView children whose bounds exceed parent's bounds
- Returns `None` only if no node in the entire tree contains the point

### Theme

- Forced light theme explicitly (`follow_system_theme: false`, `default_theme: Theme::Light`)
- Selection text: `Color32::from_rgb(0, 150, 0)` (dark green)
- Hover text: `Color32::RED`
- Image selection overlay: green stroke, hover overlay: red stroke

### Capture Lifecycle

#### ADB Capture
- Kills atx-agent + uiautomator only if `U2_STARTED` was true (avoid unnecessary overhead for pure ADB sessions)
- Resets `U2_STARTED` to false

#### U2 Capture
- First capture: kills any existing atx-agent → starts fresh → waits 2s → verifies pidof
- Subsequent captures: reuses running atx-agent
- If atx-agent died mid-session: capture fails → resets `U2_STARTED` → next attempt restarts

#### Exit Cleanup (`main()`)
- Reads `U2_SERIAL` mutex: only cleans (kill atx-agent, am force-stop uiautomator, remove adb forward) if U2 was used this session

#### Temp Files
- `std::env::temp_dir()` → `uiviewer_{adb|u2}_screenshot[_dN].png` / `uiviewer_{adb|u2}_dump[_dN].xml`
- `_dN` suffix when `display_id > 0` (multi-display)
- On success: tracked in `temp_screenshot`/`temp_xml`
- On new capture: old temps cleaned first
- On error: cleanup partial files; rollback preserves old temps
- On app exit: `Drop::drop` removes both temps

## Common Commands

```sh
cargo build
cargo run
```

Device detection: `adb devices` → picks the first connected device. No hard-coded serial.

## Windows-Specific Considerations

### Console Window Suppression
On Windows, every `adb` subprocess spawn (e.g. `adb devices`, `adb shell`,
`screencap`, `adb pull`) would flash a console window briefly because the parent
process has no console (detached via `FreeConsole()`). All ADB invocations go
through the `adb()` helper (`main.rs:37`) which sets `CREATE_NO_WINDOW` on Windows
to suppress this.

### Device Refresh Caching
`refresh_devices()` is called at the start of every `ui()` frame but is rate-limited
to once every 15 seconds (using `last_adb_check: Option<Instant>`). This prevents
spawning `adb devices` on every mouse move / repaint. A manual 🔄 button in the
toolbar resets `last_adb_check` to `None` and triggers an immediate refresh.

### ADB calls
All 28+ `Command::new("adb")` calls in the codebase have been replaced with `adb()`,
ensuring consistent `CREATE_NO_WINDOW` behavior across the entire app on Windows.
On non-Windows platforms, `adb()` is a no-op (identical to `Command::new("adb")`).
