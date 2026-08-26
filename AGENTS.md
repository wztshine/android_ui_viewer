# uiviewer — Android UI Inspection Tool

## Description

Rust GUI tool that lets you inspect Android UI layouts from uiautomator XML dumps.
Displays a screenshot with hover/click element highlighting, a collapsible tree view of
the UI hierarchy, and attribute inspection. Supports ADB capture (screencap + dump),
uiautomator2 capture (u2.jar JSON-RPC), manual file loading, and save-as.

## Tech Stack

- **Language**: Rust
- **GUI**: eframe/egui 0.35
- **XML parsing**: roxmltree 0.19
- **Image loading**: image 0.25 (png, jpeg)
- **File dialogs**: rfd 0.14
- **JSON**: serde_json 1.0
- **ADB**: via `adb()` helper (`CREATE_NO_WINDOW` on Windows)
- **Rendering**: glow backend (OpenGL)

## Project Layout

```
/var/my_share/projects/uiviewer/
├── src/main.rs      # app source
├── Cargo.toml
├── AGENTS.md
└── README.md
```

## Architecture

### Core Types

- **`UiNode`** — tree node from XML: `bounds`, `attrs`, `children`, precomputed `label`
  - `find_branch(pos, path)` — finds the smallest-area node at image pixel (walks children first, then checks own bounds; handles child bounds exceeding parent)
  - `node_at(path)` — follows `Vec<usize>` path to a node
- **`App`** — egui app state with screenshot/texture, paths, expanded set, selection/hover state
- **`CaptureMethod`** — `Adb` | `U2V3` (user-facing capture methods; the toolbar's u2 button maps to `U2V3`)
- **`TempGuard`** — RAII temp file cleanup: tracks files during capture, removes on Drop/error, `disarm()` on success
- **`DeviceRefresh`** — background device-list + display result (`devices`, `selected`, `displays`) sent via mpsc

### Key Functions

| Function | Role |
|---|---|
| `parse_xml(xml)` | Parse standard uiautomator dump into `UiNode` tree; handles multiple top-level `<node>` via `merge_nodes` |
| `parse_windows_xml(text, display_id)` | Parse `--windows` format (`<displays>` / hybrid / direct `<display>` children), extract nodes for given display |
| `get_display_ids_from_xml(text)` | Detect multi-display format and return list of display IDs from XML |
| `merge_nodes(nodes)` | Wrap multiple root nodes in a synthetic `FrameLayout` root |
| `load_texture(path, ctx)` | Load image into egui texture; format detected from file **content** (`with_guessed_format`), not extension — handles JPEG-in-`.png` (U2 v3) and mismatched user files |
| `get_displays(serial)` | Query device for `Vec<(logical_id, physical_id)>` via `dumpsys display` + `dumpsys SurfaceFlinger --display-id` |
| `adb_capture(serial, display_id, display_physical, png, xml)` | ADB capture: `screencap -p [-d <phys>]` + `uiautomator dump [--windows]` + `adb pull`; `-d` only for secondary (logical id > 0); writes to given unique temp paths |
| `uiautomator2_v3_capture(serial, display_id, display_physical, png, xml)` | U2 capture: u2.jar lifecycle, `takeScreenshot` (primary)/`dumpWindowHierarchy` via JSON-RPC; secondary displays use ADB `screencap -d <phys>` for the image |
| `launch_u2v3(serial)` | Spawn u2.jar `app_process` server and poll `/ping` (trimmed match, 30s deadline); failure cleanup is ownership-guarded (`stop_u2v3_if_current`) so a zombie thread can't kill a newer capture's server |
| `stop_u2v3()` | Kill the streaming adb shell to terminate the remote app_process |
| `stop_u2v3_if_current(id)` | Ownership-guarded stop: only kills the stored child if its id matches (zombie-thread safety) |
| `http_request(addr, method, path, body, read_timeout)` | Raw HTTP/1.1 over TCP with per-call read timeout (`HTTP_READ_TIMEOUT` 30s for JSON-RPC, `U2V3_PING_TIMEOUT` 2s for `/ping` probes so a hung probe can't exceed the launch deadline); used by u2.jar JSON-RPC + `/ping` |
| `http_jsonrpc(method, params)` | JSON-RPC POST to u2 v3 server (`/jsonrpc/0`) |
| `render_tree(ui, node, ...)` | Recursively render collapsible tree with arrows, indentation, colored labels |
| `build_label(attrs)` | Format node label `ClassName "text" [resource-id]`; precomputed once at parse time into `UiNode.label` (zero per-frame allocation) |
| `start_capture(method)` / `poll_capture` | Background-thread capture: records `U2V3_SERIAL` on the main thread, spawns thread + mpsc channel, polls `(CaptureMethod, CaptureResult)` each frame |
| `refresh_devices` / `poll_device_refresh` | Background-thread device/display refresh (15s rate-limited, 15s `REFRESH_TIMEOUT` safety net), mpsc result applied off-thread; adb calls bounded via `adb_output_bounded` so a hung adb can't leak a zombie thread |
| `next_temp_id()` | `AtomicU64` counter for unique temp file names (`TEMP_COUNTER`) |

### Layout Structure

```
CentralPanel                              — Screenshot image with hover/click/drag overlays
Panel::right("properties_panel")          — Node Tree + Properties panels (resizable)
  ├── display selector (tree_display_id)
  ├── tree scroll area
  └── properties scroll area
Panel::top("toolbar")                     — Load/ADB/U2/Save buttons + display selector + file names
Panel::bottom("status")                   — Status messages
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
- **Display selector**: ComboBox in toolbar (`display_id`, for capture/tap/swipe) and Node Tree panel (`tree_display_id`, for tree filtering only)
- **ADB**: multi-display uses `screencap -d <phys_id>` + `uiautomator dump --windows`; single uses `screencap -p` + standard `uiautomator dump`
- **U2**: screenshot via JSON-RPC `takeScreenshot` (primary) or ADB `screencap` (secondary); hierarchy via `dumpWindowHierarchy` (primary display only) — neither engine isolates a secondary display's layout, so the layout is each method's best effort
- **File parsing**: `parse_windows_xml` handles `<hierarchy><displays><display>`, hybrid `<hierarchy><display>`, and `--windows` `<displays><display><window><hierarchy>` formats
- **On display change**: for file-loaded data, re-parses XML from `file_xml_content` with `tree_display_id` (cached by `parsed_tree_display_id` to avoid per-frame re-parse); for device capture, re-captures with `display_id`
- **After capture**: `tree_display_id` set to captured `display_id` while `parsed_tree_display_id` is left stale, so the ui() re-parse guard re-parses the multi-display XML for the captured display (prevents showing `file_displays[0]`'s tree under the wrong label)

### Properties Panel

- `Panel::right("properties_panel")` (resizable, min 80px) — replaces old manual divider drag inside CentralPanel
- Labels on separate lines (key bold, value selectable+wrap)

### find_branch Behavior

- **Smallest area** selection: prefers most specific (innermost) element at click point
- **Ancestor-boundary tolerance**: checks children regardless of whether current node's bounds contain the point; handles cases like WebView/ComposeView children whose bounds exceed parent's bounds
- Returns `None` only if no node in the entire tree contains the point

### Theme

- Forced light theme explicitly (`follow_system_theme: false`, `default_theme: Theme::Light`)
    → Now via `cc.egui_ctx.set_theme(egui::Theme::Light)` (eframe 0.35)
- Selection text: `Color32::from_rgb(0, 150, 0)` (dark green)
- Hover text: `Color32::RED`
- Image selection overlay: green stroke, hover overlay: red stroke

### Capture Lifecycle

Captures run on a **background thread**: `start_capture(method)` spawns the thread and an mpsc
channel; `poll_capture` polls the result every frame. The UI stays responsive and toolbar
buttons are disabled while `capturing`.

#### Background execution
- `start_capture`: records `U2V3_SERIAL` on the main thread (so exit cleanup works even if the app closes while the thread starts), generates unique temp paths, stores them in `in_flight_screenshot`/`in_flight_xml`, spawns thread, sends `(CaptureMethod, CaptureResult)` via mpsc, requests repaint on completion
- `poll_capture`: polls the channel; Empty → `ctx.request_repaint_after` schedules a wake-up at the `CAPTURE_TIMEOUT` (30s) deadline (fires even without user input); Ok → load + track temps; Err/timeout/Disconnected → restore previous temps and clear `in_flight_*`
- Timeout: drops the receiver so the zombie thread's eventual send fails and its SendError arm self-cleans its own files; also calls `stop_u2v3()` so a zombie U2V3 thread's HTTP calls fail fast instead of hitting a newer capture's server. Disconnected also calls `stop_u2v3()` to kill any server orphaned by a panicked thread

#### ADB Capture
- `screencap` + `uiautomator dump [--windows]` + `adb pull`
- Stops any running u2.jar server before capturing

#### U2 Capture (u2.jar)
- Requires `u2.jar` on device (`python -m uiautomator2 init`); the toolbar's u2 button uses this method
- Each capture: removes stale forward, creates fresh `tcp:9008` forward, then `launch_u2v3` (spawns a fresh `app_process` and polls `/ping` with a 30s deadline)
- Primary display screenshot via JSON-RPC `takeScreenshot` → base64 JPEG (MIME-style, newline every 76 chars — whitespace stripped before decoding); any takeScreenshot failure (RPC error / non-string / empty / undecodable) falls back to ADB screencap; secondary displays use ADB `screencap -d <phys>` for the image (layout stays primary-only `dumpWindowHierarchy`)
- Hierarchy via `dumpWindowHierarchy`, retried up to 3× when the server returns empty

#### Exit Cleanup
- `main()`: reads `U2V3_SERIAL` mutex — `stop_u2v3()` + `pkill -f u2.jar` + remove `tcp:9008` forward, only if u2.jar was used this session; the serial is recorded in `start_capture` on the main thread
- `App::drop`: removes tracked temp files (`temp_*` + `pending_old_*` + `in_flight_*`), covering exits mid-capture and zombies left after timeout

#### Temp Files
- `std::env::temp_dir()` → `uiviewer_{adb|u2v3}_{screenshot|dump}_{id}.png/.xml`, unique `id` from `TEMP_COUNTER` (paths generated in `start_capture` so Drop can clean them)
- Success: previous unique files removed, new files tracked in `temp_screenshot`/`temp_xml`
- Error: thread `TempGuard` removes partial files; previous temps restored
- Timeout: previous temps restored; zombie thread self-cleans via SendError once its adb returns; `in_flight_*` covers exit-time cleanup
- User load: `load_screenshot`/`load_xml` remove the tracked temp only after the new file loads successfully (a failed load keeps the current screenshot/XML usable)

## Common Commands

```sh
cargo build
cargo run
```

Device detection: `adb devices` → picks the first connected device. No hard-coded serial.

## Requirements

- Rust **1.92+** (MSRV — eframe 0.35 requires 1.92)

## Windows-Specific Considerations

### Console Window Suppression
On Windows, every `adb` subprocess spawn (e.g. `adb devices`, `adb shell`,
`screencap`, `adb pull`) would flash a console window briefly because the parent
process has no console (detached via `FreeConsole()`). All ADB invocations go
through the `adb()` helper (`main.rs:75`) which sets `CREATE_NO_WINDOW` on Windows
to suppress this.

### Device Refresh Caching
`refresh_devices()` is called at the start of every `ui()` frame but is rate-limited
to once every 15 seconds (using `last_adb_check: Option<Instant>`). The adb calls
(`adb devices` + 2× `dumpsys`) run on a **background thread** via `fetch_device_refresh`,
and the result is applied off-thread by `poll_device_refresh`:
- Each adb call is bounded by `adb_output_bounded` (4s `REFRESH_ADB_TIMEOUT`), so a wedged
  adb can't leak a permanently hung thread; on `adb devices` timeout/failure the current
  device list is kept instead of being cleared
- A `refresh_rx` in flight blocks duplicate concurrent refreshes (manual 🔄 button is deduped too)
- A `selected == result.selected` guard skips applying displays fetched for a stale device selection
- A hung refresh is abandoned after `REFRESH_TIMEOUT` (15s) as a safety net: the receiver is
  dropped so `refresh_rx` can't block future refreshes, and a status error is shown
A manual 🔄 button in the toolbar resets `last_adb_check` to `None` and triggers an immediate refresh.

### ADB calls
All 20+ `Command::new("adb")` calls in the codebase have been replaced with `adb()`,
ensuring consistent `CREATE_NO_WINDOW` behavior across the entire app on Windows.
On non-Windows platforms, `adb()` is a no-op (identical to `Command::new("adb")`).
