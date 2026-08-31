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
├── README.md        # bilingual (EN/ZH) overview
└── README.zh.md     # Chinese-only overview
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
| `parse_xml(xml)` | Parse standard uiautomator dump into `UiNode` tree; wraps multiple top-level `<node>` via `merge_nodes` |
| `parse_windows_xml(text, display_id)` | Parse `--windows` formats (`<displays>` / hybrid / direct children), extract nodes for given display |
| `get_display_ids_from_xml(text)` | Detect multi-display format and list display IDs from XML |
| `merge_nodes(nodes)` | Wrap multiple root nodes in a synthetic `FrameLayout` root |
| `load_texture(path, ctx)` | Load image into egui texture; format from file **content**, not extension — handles JPEG-in-`.png` (U2 v3) |
| `get_displays(serial)` | Pair logical IDs (`dumpsys display`) with physical IDs (`dumpsys SurfaceFlinger --display-id`) |
| `adb_capture(...)` | ADB capture (see "ADB Capture"): concurrent screencap + single exec-out hierarchy dump trimmed to the `<?xml` prologue |
| `uiautomator2_v3_capture(...)` | U2 capture (see "U2 Capture"): `ensure_u2v3_running` warm path, then concurrent screenshot + hierarchy fetch |
| `ensure_u2v3_running(serial)` | Warm server reuse check (`u2v3_forward_matches` + `/ping`); falls back to `launch_u2v3` cold start |
| `launch_u2v3(serial)` | Cold start: jar probe + fresh `tcp:9008` forward + `app_process`, `/ping` polled until `CAPTURE_TIMEOUT` deadline |
| `release_u2v3_resources(serial)` | Stop u2.jar server + drop forward **only for sessions this app created** (`U2V3_MANAGED_SERIALS`); on device switch / disappearance |
| `stop_u2v3()` / `stop_u2v3_if_current(id)` | Kill the streaming adb shell / ownership-guarded stop (zombie-thread safety) |
| `http_request(addr, method, path, body, read_timeout)` | Raw HTTP/1.1 with per-call read timeout (`HTTP_READ_TIMEOUT` = `CAPTURE_TIMEOUT` for JSON-RPC; `PROBE_READ_TIMEOUT` 2s for probes; `U2V3_PING_TIMEOUT` 2s for `/ping`) |
| `http_jsonrpc(method, params)` | JSON-RPC POST to `/jsonrpc/0`; `http_jsonrpc_with_timeout` adds an explicit read timeout (keep-monitor probe) |
| `render_tree(ui, node, ...)` | Recursively render collapsible tree with arrows, indentation, colored labels |
| `build_label(attrs)` | Precompute node label `ClassName "text" [resource-id]` once at parse time |
| `start_capture(method)` / `poll_capture` | Background-thread capture: record `U2V3_SERIAL`, spawn thread + mpsc, poll result each frame (see "Capture Lifecycle") |
| `refresh_devices` / `poll_device_refresh` | Background-thread device/display refresh (see "Device Refresh"): 15s rate-limit, `adb_output_bounded`, `REFRESH_TIMEOUT` safety net |
| `next_temp_id()` | `AtomicU64` counter for unique temp file names (`TEMP_COUNTER`) |

### Layout Structure

```
CentralPanel                              — Screenshot image with hover/click/drag overlays
Panel::right("properties_panel")          — Node Tree + Properties panels (resizable)
  ├── display selector (tree_display_id)
  ├── tree scroll area (hscroll enabled)
  └── properties scroll area
Panel::top("toolbar")                     — Load/ADB/U2/Save buttons + display selector + file names
Panel::bottom("status")                   — Status messages
```

The right panel is sized by egui's content-driven `Panel` API: the panel's stored
width follows its content's `min_rect` (egui panel.rs reassigns the persisted rect
from the rendered frame rect). The tree `ScrollArea` is vertical-only with
`auto_shrink([true, false])`, which makes a disabled axis follow the content width —
so expanding deep nodes (long indent + unwrapped labels) would grow the panel and
squeeze the central image. `.hscroll(true)` keeps the disabled axis bounded by the
panel's available width (ScrollArea sizes to `min(available, content)` on an enabled
axis), so the divider stays where the user dragged it and wide tree rows instead
scroll horizontally.

### Tree Click Handling (critical pattern)

Tree items use a **plain `Label` (no `Sense`)** to avoid ScrollArea auto-scrolling.
Click detection is done entirely via raw input:

1. Each node's screen-space `rect` is recorded in `node_rects: Vec<(Vec<usize>, Rect)>`
2. After tree renders, `ui.input(|i| i.pointer.any_click())` checks for clicks
3. `interact_pos()` gives the click position; **gated by `ui.clip_rect().contains(pos)`** —
   only clicks inside the visible tree viewport are honored
4. Matched against `node_rects`
5. Arrow expand/collapse uses `Label::sense(Sense::click())` and does NOT cause jumps

This avoids `selectable_label` and any interactive widget inside ScrollArea that would
trigger auto-scroll-to-focused-widget behavior.

The viewport gate matters because a scrolled-out tree row still has a live
screen-space rect that extends past the viewport bottom into the Properties area
below — without the gate, a click there would phantom-select a hidden node.

### Image Interaction

- **Click**: `response.clicked()` → `selected_path` + `scroll_to_target` → green highlight + tree scroll + ancestor auto-expand. Same-spot click cycles up through ancestors.
- **Double-click**: `response.double_clicked()` → `pending_tap` → `SETTLE_DELAY` (1s) settle → re-capture. Sends `input tap [--display <id>]` to device.
- **Drag**: `Sense::click_and_drag()` → `drag_start_img` → on release with distance ≥ 10px, sends `input swipe [--display <id>] <x1> <y1> <x2> <y2>` → `SETTLE_DELAY` (1s) settle → re-capture.
- **Hover**: `response.hover_pos()` → `find_branch()` → `hovered_path` → red highlight + property preview. Skipped if mouse hasn't moved ≥ 5px (`last_hover_img_pos`).

### Multi-Display Support

- **Display detection**: `get_displays(serial)` returns `Vec<(u32, u64)>` pairing logical IDs (from `dumpsys display`) with physical IDs (from `dumpsys SurfaceFlinger --display-id`)
- **Display selector**: ComboBox in toolbar (`display_id`, for capture/tap/swipe) and Node Tree panel (`tree_display_id`, for tree filtering only)
- **ADB**: `screencap -d <phys_id>` only for secondary displays; hierarchy dump always tries `uiautomator dump --windows` first (all-window coverage, matching the U2 engine) with plain-dump fallback on older Android
- **U2**: screenshot via JSON-RPC `takeScreenshot` (primary) or ADB `screencap` (secondary); hierarchy via `dumpWindowHierarchy` (primary display only) — neither engine isolates a secondary display's layout, so the layout is each method's best effort. Success status appends "(screenshot via ADB)" (`ShotSource`) whenever the screenshot actually came from `screencap` (secondary display or RPC fallback)
- **File parsing**: `parse_windows_xml` handles `<hierarchy><displays><display>`, hybrid `<hierarchy><display>`, and `--windows` `<displays><display><window><hierarchy>` formats
- **On display change**: for file-loaded data, re-parses XML from `file_xml_content` with `tree_display_id` (cached by `parsed_tree_display_id` to avoid per-frame re-parse); for device capture, re-captures with `display_id`
- **After capture**: `load_captured_pair` parses the dump for the current `tree_display_id` (falling back to `file_displays[0]` when it's not in the dump) and sets `tree_display_id` == `parsed_tree_display_id` to that value, so the ui() re-parse guard does not fire — the tree is already built for the correct display (prevents showing `file_displays[0]`'s tree under the wrong label)

### Properties Panel

- `Panel::right("properties_panel")` (resizable, min 80px) — replaces old manual divider drag inside CentralPanel
- Labels on separate lines (key bold, value selectable+wrap)
- **XPath box**: `generate_xpath` builds an `//*[@attr="value"]` expression for the hovered/selected node, shown above the attribute list; cached by `(tree_revision, path)` in `xpath_cache` so hover repaints don't re-walk the tree every frame; capped at `XPATH_MAX_HEIGHT` (90px) with its own scroll so long text/content-desc can't squeeze the attributes out of view
- **Export Icon**: `✂️ Export Icon` button crops the selected node's `bounds` from the screenshot (`crop_screenshot`) and saves as PNG, default filename from `export_icon_name` (resource-id/text/class)

### find_branch Behavior

- **Smallest area** selection: prefers most specific (innermost) element at click point
- **Ancestor-boundary tolerance**: checks children regardless of whether current node's bounds contain the point; handles cases like WebView/ComposeView children whose bounds exceed parent's bounds
- Returns `None` only if no node in the entire tree contains the point

### Theme

- Forced light theme (`cc.egui_ctx.set_theme(egui::Theme::Light)`)
- Selection text: `Color32::from_rgb(0, 150, 0)` (dark green)
- Hover text: `Color32::RED`
- Image selection overlay: green stroke, hover overlay: red stroke

### Capture Lifecycle

Captures run on a **background thread**: `start_capture(method)` spawns the thread and an mpsc
channel; `poll_capture` polls the result every frame. The UI stays responsive and toolbar
buttons are disabled while `capturing`.

#### Background execution
- `start_capture`: records `U2V3_SERIAL` on the main thread (so exit cleanup works even if the app closes while the thread starts), generates unique temp paths, stores them in `in_flight_screenshot`/`in_flight_xml`, spawns thread, sends `(CaptureMethod, CaptureResult)` via mpsc, requests repaint on completion
- `poll_capture`: polls the channel; Empty → `ctx.request_repaint_after` schedules a wake-up at the `CAPTURE_TIMEOUT` (10s) deadline (fires even without user input); Ok → load + track temps; Err/timeout/Disconnected → restore previous temps and clear `in_flight_*`
- Timeout: drops the receiver so the zombie thread's eventual send fails and its SendError arm self-cleans its own files; also calls `stop_u2v3()` so a zombie U2V3 thread's HTTP calls fail fast instead of hitting a newer capture's server. Disconnected also calls `stop_u2v3()` to kill any server orphaned by a panicked thread

#### ADB Capture
- Screenshot (`screencap`) and hierarchy dump run **concurrently** on two adb connections; the hierarchy is a single `exec-out` round trip (`uiautomator dump --windows ||` plain dump `; cat; rm -f`) replacing the old separate dump/`adb pull`/`rm` calls
- Stops any running u2.jar server before capturing (the accessibility connection is exclusive — `uiautomator dump` conflicts with a live u2 server), so alternating ADB → U2 pays the U2 cold start again

#### U2 Capture (u2.jar)
- Requires `u2.jar` on device (`python -m uiautomator2 init`); the toolbar's u2 button uses this method
- Server kept alive across captures: each capture first runs `ensure_u2v3_running` — forward ownership for the current serial (`u2v3_forward_matches`, guards against multi-device stale forwards routing 127.0.0.1:9008 to the wrong phone) + `/ping` probe (`u2v3_alive`). Only when either fails does `launch_u2v3` cold-start: jar probe → fresh forward → new `app_process` → `/ping` polled every 100ms with a `CAPTURE_TIMEOUT` (10s) deadline. Warm captures skip jar probe, forward setup and server spawn entirely
- Ownership attribution: serials whose forward+server this app created are tracked in `U2V3_MANAGED_SERIALS`; all cleanup paths (device-switch release, cold-start sweep, exit cleanup) only touch those. A server started by another tool (e.g. python-uiautomator2, which shares the tcp:9008 convention) is transparently reused via the warm path and left alive on switch/exit; taking over the *target* device itself (unconditional pkill + same-spec forward remove before launch) remains intentional
- Screenshot (`takeScreenshot` / screencap fallback) and hierarchy (`dumpWindowHierarchy`) are fetched concurrently; if the server serializes requests internally this merely degrades to sequential timing
- Primary display screenshot via JSON-RPC `takeScreenshot` → base64 JPEG (MIME-style, newline every 76 chars — whitespace stripped before decoding); any takeScreenshot failure (RPC error / non-string / empty / undecodable) falls back to ADB screencap; secondary displays use ADB `screencap -d <phys>` for the image (layout stays primary-only `dumpWindowHierarchy`)
- Hierarchy via `dumpWindowHierarchy`, retried up to 2× when the server returns empty

#### Exit Cleanup
- `main()`: reads `U2V3_SERIAL` mutex — `stop_u2v3()` + `pkill -f u2.jar` + remove `tcp:9008` forward, only if u2.jar was used this session; the serial is recorded in `start_capture` on the main thread
- `App::drop`: removes tracked temp files (`temp_*` + `pending_old_*` + `in_flight_*`), covering exits mid-capture and zombies left after timeout

#### Temp Files
- `std::env::temp_dir()` → `uiviewer_{adb|u2v3}_{screenshot|dump}_{id}.png/.xml`, unique `id` from `TEMP_COUNTER` (paths generated in `start_capture` so Drop can clean them)
- Success: previous unique files removed, new files tracked in `temp_screenshot`/`temp_xml`
- Error: thread `TempGuard` removes partial files; previous temps restored
- Timeout: previous temps restored; zombie thread self-cleans via SendError once its adb returns; `in_flight_*` covers exit-time cleanup
- User load: `load_screenshot`/`load_xml` remove the tracked temp only after the new file loads successfully (a failed load keeps the current screenshot/XML usable)

### Device Refresh

`refresh_devices()` is called at the start of every `ui()` frame but is rate-limited
to once every 15 seconds (using `last_adb_check: Option<Instant>`). The adb calls
(`adb devices` + 2× `dumpsys`) run on a **background thread** via `fetch_device_refresh`,
and the result is applied off-thread by `poll_device_refresh`:
- Each adb call is bounded by `adb_output_bounded` (3s `REFRESH_ADB_TIMEOUT`), so a wedged
  adb can't leak a permanently hung thread; on `adb devices` timeout/failure the current
  device list is kept instead of being cleared
- A `refresh_rx` in flight blocks duplicate concurrent refreshes (manual 🔄 button is deduped too)
- A `selected == result.selected` guard skips applying displays fetched for a stale device selection
- A hung refresh is abandoned after `REFRESH_TIMEOUT` (15s) as a safety net: the receiver is
  dropped so `refresh_rx` can't block future refreshes, and a status error is shown
A manual 🔄 button in the toolbar resets `last_adb_check` to `None` and triggers an immediate refresh.

### Keep Monitor (Auto Capture)

Toolbar checkbox + interval stepper (`-`/`+` buttons, 0.5s steps, clamped 0.5–60s; changes reschedule the next tick immediately). Checking it fires a capture **immediately** and disables the two capture buttons; unchecking (or any capture failure) calls `stop_keep_monitor()` which clears the schedule, probe state, and checkbox.

- **Method/source**: uses `last_capture` (the previously used method); `start_capture` reads the currently selected device + display id at fire time. Ticks yield while `pending_tap`/`tap_settle_start` is active so user tap/swipe feedback recaptures are never displaced.
- **Scheduling**: next tick is scheduled from each capture *completion* (`poll_capture` Ok arm) — natural backpressure when a capture outlasts the interval.
- **Wake-ups (event-driven)**: the capture thread and the U2 probe thread both call `ctx.request_repaint()` once their result is ready; an idle wait between ticks arms a single precise `request_repaint_after(next_auto_capture - now)` wake-up instead. No heartbeat or polling keeps the UI awake, so monitoring costs zero repaints while idle — even a probe whose result is "unchanged" triggers exactly one repaint to be handled.
- **Two-tier change detection (U2 method only)**: each tick first runs an off-thread hierarchy-only RPC probe (`probe_u2_hierarchy_hash`, result hashed with FNV-1a). Full capture fires only when the hash changed, every `MONITOR_FORCE_REFRESH_EVERY` unchanged probes (eventual consistency for pixel-only changes like video), or when the probe fails (then the real capture surfaces the error or recovers). ADB method always captures fully (its probe would cost the same JVM spawn).
- **Tree panel**: monitor-driven captures never retarget `tree_display_id` (`capture_source_auto` flag distinguishes them from user-driven ones); successful captures made while monitoring refresh the baseline hash from the fresh dump.

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
process has no console (detached via `FreeConsole()`). All 20+ `adb` invocations
in the codebase go through the `adb()` helper (`main.rs:161`), which sets
`CREATE_NO_WINDOW` on Windows to suppress this; on other platforms `adb()` is a
no-op (identical to `Command::new("adb")`).

### Logging
The `log!` macro (`main.rs:69`) writes to stderr and appends to `<cwd>/uiviewer.log`
(`uiviewer_log_path`, `main.rs:38`), capped at `LOG_MAX_BYTES` (10MB) by truncate-and-reuse.
Shared behind `LOG_MUTEX` so concurrent background-thread calls append atomically;
best-effort, never panics on write failure.
