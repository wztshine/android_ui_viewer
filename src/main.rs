use eframe::egui;
use egui::{
    pos2, Color32, Pos2, Rect, Sense, Stroke, TextureHandle, TextureOptions, Vec2,
};
use roxmltree::Document;
use std::collections::HashSet;
use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::Instant;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

static U2V3_SERIAL: Mutex<Option<String>> = Mutex::new(None);
static U2V3_PROCESS: Mutex<Option<(u64, std::process::Child)>> = Mutex::new(None);
// Serials whose u2.jar session THIS app created (server + host forward).
// Cleanup must only ever touch these: port 9008 is the u2 ecosystem standard,
// so other tools (e.g. python-uiautomator2) may legitimately own the forward
// or the on-device server — blindly sweeping it would break their session.
static U2V3_MANAGED_SERIALS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static U2V3_SERVER_ID: AtomicU64 = AtomicU64::new(0);
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_temp_id() -> u64 {
    TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
}

// Log file path: <current working dir>/uiviewer.log. The app detaches from the
// console (FreeConsole) on Windows, so eprintln goes nowhere there; a file log
// next to the executable keeps the [uiviewer] diagnostics observable.
fn uiviewer_log_path() -> PathBuf {
    PathBuf::from("uiviewer.log")
}

// Fixed cap for the log file: once exceeded the file is truncated and reuse
// starts from the top, so it can never grow unbounded.
const LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;

// Shared guard so concurrent background-thread log calls append atomically.
static LOG_MUTEX: Mutex<()> = Mutex::new(());

// Append a line to the log file, truncating it first if it has reached
// LOG_MAX_BYTES (fixed-size overwrite). Best-effort: never panics, a failure
// to open/write the log must not break the app.
fn append_log(msg: &str) {
    let _g = LOG_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let path = uiviewer_log_path();
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) >= LOG_MAX_BYTES {
        let _ = std::fs::File::create(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{msg}");
    }
}

// Log a message to stderr AND append it to the log file.
macro_rules! log {
    ($($arg:tt)*) => {{
        eprintln!($($arg)*);
        append_log(&format!($($arg)*));
    }};
}

const U2V3_ADDR: &str = "127.0.0.1:9008";
const U2V3_FORWARD: &str = "tcp:9008";
const U2V3_JAR: &str = "/data/local/tmp/u2.jar";
const U2V3_PORT: &str = "9008";
// Main class of the u2.jar app_process server; used by the launch command and
// the pkill cleanup (the CLASSPATH is an env var, so pkill must match this argv).
const U2V3_MAIN_CLASS: &str = "com.wetest.uia2.Main";
const DEVICE_DUMP: &str = "/sdcard/uiviewer_dump.xml";
const CAPTURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const REFRESH_ADB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

// HTTP read timeout for JSON-RPC calls (takeScreenshot can take seconds).
const HTTP_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
// Short read timeout for /ping probes: a single hung probe must not exceed the
// launch deadline, which is only checked between attempts.
const U2V3_PING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
// Poll interval while waiting for a cold-started u2.jar server to answer /ping.
// A short interval keeps perceived startup latency close to the actual boot
// time instead of quantizing it to the poll period (was 500ms).
const U2V3_PING_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
// Keep-monitor two-tier probing (U2 method): a hierarchy-only RPC is hashed
// each tick; full captures run only when the hash changed, or every
// MONITOR_FORCE_REFRESH_EVERY unchanged probes as an eventual-consistency
// guarantee (XML cannot see pixel-only changes like video/canvas content).
const MONITOR_FORCE_REFRESH_EVERY: u32 = 10;
// FNV-1a 64-bit constants for cheap XML change detection.
const FNV1A_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A_PRIME: u64 = 0x0000_0100_0000_01b3;
// Keep-monitor liveness heartbeat (ms): while monitoring, guarantee periodic
// frames so a missed repaint request anywhere can never freeze the UI until
// the next input event arrives.
const MONITOR_HEARTBEAT_MS: u64 = 100;
// JSON-RPC params: takeScreenshot(display_id, quality) — numeric display id,
// 0 = primary; returns base64 JPEG. dumpWindowHierarchy(compressed, max_depth)
// mirrors uiautomator2's dump_hierarchy(compressed=False, max_depth=50).
const U2V3_PRIMARY_DISPLAY_ID: i32 = 0;
const U2V3_SCREENSHOT_QUALITY: i32 = 80;
const U2V3_DUMP_COMPRESSED: bool = false;
const U2V3_DUMP_MAX_DEPTH: i32 = 50;

struct TempGuard {
    files: Vec<PathBuf>,
}

impl TempGuard {
    fn new() -> Self {
        Self { files: Vec::new() }
    }
    fn track(&mut self, p: PathBuf) {
        self.files.push(p);
    }
    fn disarm(mut self) {
        self.files.clear();
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        for f in self.files.drain(..) {
            let _ = std::fs::remove_file(&f);
        }
    }
}

#[derive(Clone)]
struct UiNode {
    bounds: Rect,
    attrs: Vec<(String, String)>,
    children: Vec<UiNode>,
    label: String,
}

fn adb() -> Command {
    #[cfg(target_os = "windows")]
    {
        let mut cmd = Command::new("adb");
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    }
    #[cfg(not(target_os = "windows"))]
    {
        Command::new("adb")
    }
}

impl UiNode {
    fn find_branch(&self, pos: Pos2, path: &mut Vec<usize>) -> Option<Vec<usize>> {
        let mut best_path: Option<Vec<usize>> = None;
        let mut best_area = f32::MAX;
        for (i, child) in self.children.iter().enumerate() {
            path.push(i);
            if let Some(found) = child.find_branch(pos, path) {
                let sub = &found[path.len()..];
                let area = {
                    let mut n = child;
                    for &idx in sub {
                        n = &n.children[idx];
                    }
                    n.bounds.area()
                };
                if area < best_area
                    || (area == best_area
                        && found.len() > best_path.as_ref().map_or(0, |p| p.len()))
                {
                    best_area = area;
                    best_path = Some(found);
                }
            }
            path.pop();
        }
        if best_path.is_some() {
            return best_path;
        }
        if self.bounds.contains(pos) {
            return Some(path.clone());
        }
        None
    }

    fn node_at(&self, path: &[usize]) -> Option<&UiNode> {
        let mut node = self;
        for &idx in path {
            node = node.children.get(idx)?;
        }
        Some(node)
    }
}

fn build_label(attrs: &[(String, String)]) -> String {
    let cls = attrs
        .iter()
        .find(|(k, _)| k == "class")
        .map(|(_, v)| v.rsplit('.').next().unwrap_or(v))
        .unwrap_or("?");
    let text = attrs
        .iter()
        .find(|(k, _)| k == "text")
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty() && *v != "null");
    let rid = attrs
        .iter()
        .find(|(k, _)| k == "resource-id")
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty() && *v != "null");
    let rid_short = rid.and_then(|r| r.rsplit('/').next());

    let mut s = cls.to_string();
    if let Some(t) = text {
        use std::fmt::Write;
        write!(s, " \"{t}\"").ok();
    }
    if let Some(r) = rid_short {
        use std::fmt::Write;
        write!(s, " [{r}]").ok();
    }
    if s.len() > 100 {
        let b = s.floor_char_boundary(97);
        s.truncate(b);
        s.push('…');
    }
    s
}

fn render_tree(
    ui: &mut egui::Ui,
    node: &UiNode,
    path: &[usize],
    expanded: &mut HashSet<Vec<usize>>,
    selected: Option<&[usize]>,
    hovered: Option<&[usize]>,
    node_rects: &mut Vec<(Vec<usize>, egui::Rect)>,
) {
    let is_selected = selected == Some(path);
    let is_hovered = hovered == Some(path);
    let is_expanded = expanded.contains(path);
    let has_children = !node.children.is_empty();

    ui.horizontal(|ui| {
        ui.add_space(path.len() as f32 * 18.0);

        if has_children {
            let icon = if is_expanded { "▼" } else { "▶" };
            let resp = ui.add(
                egui::Label::new(egui::RichText::new(icon).size(12.0))
                    .sense(Sense::click()),
            );
            if resp.clicked() {
                if is_expanded {
                    expanded.remove(path);
                } else {
                    expanded.insert(path.to_vec());
                }
            }
        } else {
            ui.add_space(14.0);
        }

        let label = &node.label;
        let rich = if is_selected {
            egui::RichText::new(label).color(Color32::from_rgb(0, 150, 0))
        } else if is_hovered {
            egui::RichText::new(label).color(Color32::RED)
        } else {
            egui::RichText::new(label)
        };
        let resp = ui.add(egui::Label::new(rich));
        node_rects.push((path.to_vec(), resp.rect));
    });

    if has_children && is_expanded {
        for (i, child) in node.children.iter().enumerate() {
            let mut child_path = path.to_vec();
            child_path.push(i);
            render_tree(ui, child, &child_path, expanded, selected, hovered, node_rects);
        }
    }
}

struct App {
    screenshot_texture: Option<TextureHandle>,
    root_node: Option<UiNode>,
    selected_path: Option<Vec<usize>>,
    scroll_to_target: Option<Vec<usize>>,
    hovered_path: Option<Vec<usize>>,
    screenshot_path: Option<PathBuf>,
    xml_path: Option<PathBuf>,
    expanded: HashSet<Vec<usize>>,
    last_selected: Option<Vec<usize>>,
    screenshot_dir: Option<PathBuf>,
    xml_dir: Option<PathBuf>,
    temp_screenshot: Option<PathBuf>,
    temp_xml: Option<PathBuf>,
    status_message: Option<String>,
    status_is_error: bool,
    adb_devices: Vec<String>,
    selected_device: Option<String>,
    available_displays: Vec<(u32, u64)>,
    display_id: u32,
    tree_display_id: u32,
    parsed_tree_display_id: u32,
    pending_tap: Option<(f32, f32)>,
    tap_settle_start: Option<Instant>,
    last_capture: Option<CaptureMethod>,
    keep_monitor: bool,
    monitor_interval_secs: f64,
    next_auto_capture: Option<Instant>,
    monitor_xml_hash: Option<u64>,
    monitor_probe_count: u32,
    monitor_probe_rx: Option<mpsc::Receiver<Option<u64>>>,
    click_pos: Option<Pos2>,
    last_hover_img_pos: Option<Pos2>,
    file_displays: Vec<u32>,
    file_xml_content: Option<String>,
    properties_width: f32,
    drag_start_img: Option<Pos2>,
    last_adb_check: Option<Instant>,
    capturing: bool,
    capture_start: Option<Instant>,
    capture_rx: Option<mpsc::Receiver<(CaptureMethod, CaptureResult)>>,
    pending_old_screenshot: Option<PathBuf>,
    pending_old_xml: Option<PathBuf>,
    in_flight_screenshot: Option<PathBuf>,
    in_flight_xml: Option<PathBuf>,
    refresh_rx: Option<mpsc::Receiver<DeviceRefresh>>,
    refresh_start: Option<Instant>,
    manual_refreshing: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum CaptureMethod {
    Adb,
    U2V3,
}

// How the screenshot bytes of a successful capture were produced. For the ADB
// method this is always Adb; for U2V3 it distinguishes JSON-RPC from the ADB
// paths (secondary display by design, or fallback when takeScreenshot fails).
#[derive(Clone, Copy, PartialEq)]
enum ShotSource {
    JsonRpc,
    Adb,
}

type CaptureResult = Result<(PathBuf, PathBuf, ShotSource), String>;

struct DeviceRefresh {
    devices: Vec<String>,
    selected: Option<String>,
    displays: Option<Vec<(u32, u64)>>,
    // Temporary diagnostic detail from the scan (why the device list is empty).
    diag: Option<String>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            screenshot_texture: None,
            root_node: None,
            selected_path: None,
            scroll_to_target: None,
            hovered_path: None,
            screenshot_path: None,
            xml_path: None,
            expanded: HashSet::new(),
            last_selected: None,
            screenshot_dir: None,
            xml_dir: None,
            temp_screenshot: None,
            temp_xml: None,
            status_message: None,
            status_is_error: false,
            adb_devices: Vec::new(),
            selected_device: None,
            available_displays: vec![(0, 0)],
            display_id: 0,
            tree_display_id: 0,
            parsed_tree_display_id: 0,
            pending_tap: None,
            tap_settle_start: None,
            last_capture: None,
            keep_monitor: false,
            monitor_interval_secs: 3.0,
            next_auto_capture: None,
            monitor_xml_hash: None,
            monitor_probe_count: 0,
            monitor_probe_rx: None,
            click_pos: None,
            last_hover_img_pos: None,
            file_displays: Vec::new(),
            file_xml_content: None,
            properties_width: 350.0,
            drag_start_img: None,
            last_adb_check: None,
            capturing: false,
            capture_start: None,
            capture_rx: None,
            pending_old_screenshot: None,
            pending_old_xml: None,
            in_flight_screenshot: None,
            in_flight_xml: None,
            refresh_rx: None,
            refresh_start: None,
            manual_refreshing: false,
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        for p in [
            self.temp_screenshot.take(),
            self.pending_old_screenshot.take(),
            self.in_flight_screenshot.take(),
        ]
        .into_iter()
        .flatten()
        {
            let _ = std::fs::remove_file(&p);
        }
        for p in [
            self.temp_xml.take(),
            self.pending_old_xml.take(),
            self.in_flight_xml.take(),
        ]
        .into_iter()
        .flatten()
        {
            let _ = std::fs::remove_file(&p);
        }
    }
}

fn parse_bounds(s: &str) -> Option<Rect> {
    let s = s.trim();
    let inner = s.strip_prefix('[')?.strip_suffix(']')?;
    let (first, second) = inner.split_once("][")?;
    let p1: Vec<f32> = first.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    let p2: Vec<f32> = second.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    if p1.len() != 2 || p2.len() != 2 {
        return None;
    }
    Some(Rect::from_min_max(pos2(p1[0], p1[1]), pos2(p2[0], p2[1])))
}

fn parse_node(node: &roxmltree::Node) -> Option<UiNode> {
    let bounds_str = node.attribute("bounds")?;
    let bounds = parse_bounds(bounds_str)?;

    let attrs: Vec<(String, String)> = node
        .attributes()
        .map(|a| (a.name().to_string(), a.value().to_string()))
        .collect();
    let label = build_label(&attrs);

    let children: Vec<UiNode> = node
        .children()
        .filter(|c| c.is_element())
        .filter_map(|c| parse_node(&c))
        .collect();

    Some(UiNode { bounds, attrs, children, label })
}

fn parse_xml(text: &str) -> Option<UiNode> {
    let doc = Document::parse(text).ok()?;
    let root = doc.root_element();
    if root.tag_name().name() != "hierarchy" {
        return None;
    }
    let nodes: Vec<UiNode> = root.children()
        .filter(|c| c.is_element())
        .filter_map(|c| parse_node(&c))
        .collect();
    merge_nodes(nodes)
}

fn merge_nodes(nodes: Vec<UiNode>) -> Option<UiNode> {
    if nodes.is_empty() {
        return None;
    }
    if nodes.len() == 1 {
        return Some(nodes.into_iter().next().unwrap());
    }
    let bounds = nodes.iter().fold(None, |acc: Option<Rect>, n| {
        Some(acc.map(|r| r.union(n.bounds)).unwrap_or(n.bounds))
    }).unwrap_or(Rect::from_min_max(pos2(0.0, 0.0), pos2(0.0, 0.0)));
    let attrs = vec![("class".into(), "android.widget.FrameLayout".into())];
    let label = build_label(&attrs);
    Some(UiNode {
        bounds,
        attrs,
        children: nodes,
        label,
    })
}

// Collect top-level <node> children of a plain <hierarchy> whose display-id
// attribute matches `display_id`, and merge them under a synthetic root.
// Handles multi-display dumps that carry display-id on the top-level nodes
// instead of using the <displays>/<display> wrapper. A node without a
// display-id attribute is treated as belonging to display 0 (primary).
fn parse_plain_hierarchy_display(root: &roxmltree::Node, display_id: u32) -> Option<UiNode> {
    let nodes: Vec<UiNode> = root
        .children()
        .filter(|c| c.is_element() && c.tag_name().name() == "node")
        .filter(|c| {
            c.attribute("display-id")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0)
                == display_id
        })
        .filter_map(|c| parse_node(&c))
        .collect();
    merge_nodes(nodes)
}

fn parse_windows_xml(text: &str, display_id: u32) -> Option<UiNode> {
    let doc = Document::parse(text).ok()?;
    let root = doc.root_element();
    // Accept <displays>, or <hierarchy> with <displays> or direct <display> children
    let display_node = if root.tag_name().name() == "displays" {
        root.children().filter(|c| c.is_element() && c.tag_name().name() == "display")
            .find_map(|c| {
                let id = c.attribute("id")?.parse::<u32>().ok()?;
                if id == display_id { Some(c) } else { None }
            })
    } else if root.tag_name().name() == "hierarchy" {
        // Try <hierarchy> <displays> <display>...
        let from_displays = root.children().find(|c| c.is_element() && c.tag_name().name() == "displays")
            .and_then(|displays| {
                displays.children().filter(|c| c.is_element() && c.tag_name().name() == "display")
                    .find_map(|c| {
                        let id = c.attribute("id")?.parse::<u32>().ok()?;
                        if id == display_id { Some(c) } else { None }
                    })
            });
        if from_displays.is_some() {
            from_displays
        } else {
            let direct = root.children().filter(|c| c.is_element() && c.tag_name().name() == "display")
                .find_map(|c| {
                    let id = c.attribute("id")?.parse::<u32>().ok()?;
                    if id == display_id { Some(c) } else { None }
                });
            if direct.is_some() {
                direct
            } else {
                // Plain <hierarchy> whose top-level <node>s carry a display-id
                // attribute (no <displays>/<display> wrapper). Return the
                // merged nodes for this display directly.
                return parse_plain_hierarchy_display(&root, display_id);
            }
        }
    } else {
        return None;
    };
    let display = display_node?;
    let has_window = display.children().any(|c| c.is_element() && c.tag_name().name() == "window");
    if has_window {
        let mut all_nodes: Vec<UiNode> = Vec::new();
        for window in display.children().filter(|c| c.is_element() && c.tag_name().name() == "window") {
            if let Some(hierarchy) = window.children().find(|c| c.is_element() && c.tag_name().name() == "hierarchy") {
                let nodes: Vec<UiNode> = hierarchy.children()
                    .filter(|c| c.is_element())
                    .filter_map(|c| parse_node(&c))
                    .collect();
                all_nodes.extend(nodes);
            }
        }
        return merge_nodes(all_nodes);
    }
    // Direct <node> children under <display> — collect all as siblings
    let nodes: Vec<UiNode> = display.children()
        .filter(|c| c.is_element())
        .filter_map(|c| parse_node(&c))
        .collect();
    merge_nodes(nodes)
}

fn load_texture(path: &Path, ctx: &egui::Context) -> Result<(TextureHandle, Vec2), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("open image: {e}"))?;
    let img = image::ImageReader::new(std::io::BufReader::new(file))
        .with_guessed_format()
        .map_err(|e| format!("guess image format: {e}"))?
        .decode()
        .map_err(|e| format!("decode image: {e}"))?;
    let size = Vec2::new(img.width() as f32, img.height() as f32);
    let rgba = img.to_rgba8();
    let color_image = egui::ColorImage::from_rgba_unmultiplied(
        [img.width() as usize, img.height() as usize],
        rgba.as_raw(),
    );
    let tex = ctx.load_texture("screenshot", color_image, TextureOptions::default());
    Ok((tex, size))
}

fn get_display_ids_from_xml(text: &str) -> Vec<u32> {
    let doc = match Document::parse(text) {
        Ok(d) => d,
        _ => return Vec::new(),
    };
    let root = doc.root_element();
    if root.tag_name().name() == "displays" {
        return root.children()
            .filter(|c| c.is_element() && c.tag_name().name() == "display")
            .filter_map(|c| c.attribute("id")?.parse::<u32>().ok())
            .collect();
    }
    if root.tag_name().name() == "hierarchy" {
        // <hierarchy> <displays> <display>...
        let from_displays = root.children()
            .find(|c| c.is_element() && c.tag_name().name() == "displays")
            .map(|displays| {
                displays.children()
                    .filter(|c| c.is_element() && c.tag_name().name() == "display")
                    .filter_map(|c| c.attribute("id")?.parse::<u32>().ok())
                    .collect::<Vec<u32>>()
            })
            .unwrap_or_default();
        if !from_displays.is_empty() {
            return from_displays;
        }
        // <display> directly under <hierarchy>
        let direct: Vec<u32> = root.children()
            .filter(|c| c.is_element() && c.tag_name().name() == "display")
            .filter_map(|c| c.attribute("id")?.parse::<u32>().ok())
            .collect();
        if !direct.is_empty() {
            return direct;
        }
        // Plain <hierarchy> whose top-level <node>s carry a display-id attribute
        // (multi-display uiautomator dump without the --windows wrapper). A node
        // missing the attribute is treated as display 0, mirroring the
        // get_display_ids_from_xml fallback below.
        let mut node_ids: Vec<u32> = root.children()
            .filter(|c| c.is_element() && c.tag_name().name() == "node")
            .map(|c| {
                c.attribute("display-id")
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(0)
            })
            .collect();
        node_ids.sort();
        node_ids.dedup();
        return node_ids;
    }
    Vec::new()
}

// Byte-substring search used to locate the XML prologue in exec-out output.
// Returns None when the needle cannot fit (including empty haystacks).
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn adb_capture(serial: &str, display_id: u32, display_physical: u64, png: PathBuf, xml: PathBuf) -> Result<(PathBuf, PathBuf), String> {
    let mut tmp_guard = TempGuard::new();

    // Screenshot (SurfaceFlinger) and hierarchy dump (accessibility) hit
    // disjoint device subsystems, so they run concurrently on two adb
    // connections; overall latency becomes max(a, b) instead of a + b.
    let (shot, xml_bytes) = std::thread::scope(|s| {
        let h_img = s.spawn(move || -> Result<Vec<u8>, String> {
            // The physical display id is a large opaque token for every display
            // (primary included), so "secondary" is detected via the logical id.
            let out = if display_id > 0 {
                let phys_str = display_physical.to_string();
                adb()
                    .args(["-s", serial, "exec-out", "screencap", "-p", "-d", &phys_str])
                    .output()
            } else {
                adb()
                    .args(["-s", serial, "exec-out", "screencap", "-p"])
                    .output()
            }
            .map_err(|e| format!("screencap failed: {e}"))?;
            if !out.status.success() {
                return Err("screencap returned error".into());
            }
            Ok(out.stdout)
        });
        let h_xml = s.spawn(move || -> Result<Vec<u8>, String> {
            // Single adb round trip: clear any stale dump left by a previous
            // interrupted run (so a failed dump can't serve old XML via cat),
            // try --windows first (all-window coverage, matching the U2
            // engine), fall back to the classic dump, then cat the result
            // straight back and clean up — replacing the previous separate
            // dump/pull/rm invocations. exec-out returns raw bytes (no pty
            // newline mangling); any status text uiautomator prints ahead of
            // the file content is skipped by locating "<?xml".
            let script = format!(
                "rm -f {D}; uiautomator dump --windows {D} >/dev/null 2>&1 \
                 || uiautomator dump {D} >/dev/null 2>&1; cat {D}; rm -f {D}",
                D = DEVICE_DUMP
            );
            let out = adb()
                .args(["-s", serial, "exec-out", &script])
                .output()
                .map_err(|e| format!("uiautomator dump failed: {e}"))?;
            match find_subslice(&out.stdout, b"<?xml") {
                Some(i) => Ok(out.stdout[i..].to_vec()),
                None => Err("uiautomator dump returned no XML (both --windows and fallback)".into()),
            }
        });
        (
            h_img.join().unwrap_or_else(|_| Err("screencap thread panicked".into())),
            h_xml.join().unwrap_or_else(|_| Err("dump thread panicked".into())),
        )
    });

    let img = shot?;
    tmp_guard.track(png.clone());
    std::fs::write(&png, &img).map_err(|e| format!("write png: {e}"))?;
    let xml_bytes = xml_bytes?;
    tmp_guard.track(xml.clone());
    std::fs::write(&xml, &xml_bytes).map_err(|e| format!("write xml: {e}"))?;

    tmp_guard.disarm();
    Ok((png, xml))
}

// Run an adb command with a hard deadline: stdout is redirected to a temp file
// (so a large output can't deadlock on the pipe buffer), and if the child hasn't
// exited by the deadline it is killed. Returns None on timeout/failure. Used by
// the device-refresh path so a wedged adb can't leak a permanently hung thread.
fn adb_output_bounded(args: &[&str], timeout: std::time::Duration) -> Option<std::process::Output> {
    let tmp = std::env::temp_dir().join(format!("uiviewer_adb_{}.out", next_temp_id()));
    let file = match std::fs::File::create(&tmp) {
        Ok(f) => f,
        Err(e) => {
            log!("[uiviewer] adb_output_bounded: create temp file {:?} failed: {e}", tmp);
            return None;
        }
    };
    let mut child = match adb()
        .args(args)
        .stdout(std::process::Stdio::from(file))
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            log!("[uiviewer] adb_output_bounded: spawn `adb {:?}` failed: {e}", args);
            let _ = std::fs::remove_file(&tmp);
            return None;
        }
    };
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() >= deadline {
                    // Timeout: do NOT kill the adb client. Killing a client that
                    // is mid cold-start leaves the shared adb server half
                    // initialized and corrupts it for every tool on the machine.
                    // Detach the child to a reaper thread instead: it finishes
                    // on its own and the temp file is removed once its handle
                    // is released.
                    log!(
                        "[uiviewer] adb_output_bounded: `adb {:?}` timed out after {:?}; detaching",
                        args, timeout
                    );
                    let tmp2 = tmp.clone();
                    std::thread::spawn(move || {
                        let _ = child.wait();
                        let _ = std::fs::remove_file(&tmp2);
                    });
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                log!("[uiviewer] adb_output_bounded: try_wait `adb {:?}` error: {e}", args);
                // try_wait error: same policy, let the client finish on its own
                // rather than risking a corrupted adb server for other tools.
                let tmp2 = tmp.clone();
                std::thread::spawn(move || {
                    let _ = child.wait();
                    let _ = std::fs::remove_file(&tmp2);
                });
                return None;
            }
        }
    };
    let stdout = std::fs::read(&tmp).unwrap_or_default();
    let _ = std::fs::remove_file(&tmp);
    log!(
        "[uiviewer] adb_output_bounded: `adb {:?}` ok status={:?} stdout={} bytes",
        args,
        status,
        stdout.len()
    );
    Some(std::process::Output { status, stdout, stderr: Vec::new() })
}

fn get_displays(serial: &str) -> Vec<(u32, u64)> {
    // Physical (SurfaceFlinger) and logical (display manager) display ids are
    // independent reads of disjoint device subsystems, so they are fetched
    // concurrently: worst-case latency becomes max(a, b) instead of a + b while
    // each call keeps its own timeout budget.
    let (physical_output, logical_output) = std::thread::scope(|s| {
        let h_phys = s.spawn(|| {
            adb_output_bounded(
                &["-s", serial, "shell", "dumpsys", "SurfaceFlinger", "--display-id"],
                REFRESH_ADB_TIMEOUT,
            )
            .filter(|o| o.status.success())
        });
        let h_log = s.spawn(|| {
            adb_output_bounded(
                &["-s", serial, "shell", "dumpsys", "display"],
                REFRESH_ADB_TIMEOUT,
            )
            .filter(|o| o.status.success())
        });
        (
            h_phys.join().unwrap_or(None),
            h_log.join().unwrap_or(None),
        )
    });
    let mut logical_to_physical: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
    if let Some(output) = physical_output.as_ref() {
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let line = line.trim();
            // Format: "Display <phys_id> (HWC display <log_id>): ..."
            if let Some(rest) = line.strip_prefix("Display ") {
                if let Some(phys_str) = rest.split_whitespace().next() {
                    if let Ok(phys) = phys_str.parse::<u64>() {
                        if let Some(log_str) = rest.split("(HWC display ").nth(1) {
                            if let Some(log_str) = log_str.split(')').next() {
                                if let Ok(log) = log_str.trim().parse::<u32>() {
                                    logical_to_physical.insert(log, phys);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Get logical display IDs from dumpsys display
    let mut logical_ids: Vec<u32> = if let Some(output) = logical_output.as_ref() {
        let text = String::from_utf8_lossy(&output.stdout);
        text.lines()
            .filter_map(|l| {
                let l = l.trim();
                if l.starts_with("Display Id=") {
                    l.trim_start_matches("Display Id=").trim().parse::<u32>().ok()
                } else {
                    None
                }
            })
            .collect()
    } else {
        vec![0]
    };
    logical_ids.sort();
    logical_ids.dedup();
    if logical_ids.is_empty() {
        log!("[uiviewer] get_displays: {serial}: no logical display ids, using [(0,0)]");
        return vec![(0, 0)];
    }
    let result: Vec<(u32, u64)> = logical_ids.into_iter().map(|log| {
        let phys = logical_to_physical.get(&log).copied().unwrap_or(0);
        (log, phys)
    }).collect();
    log!(
        "[uiviewer] get_displays: {serial}: phys_ok={} log_ok={} -> {:?}",
        physical_output.is_some(),
        logical_output.is_some(),
        result
    );
    result
}

fn fetch_device_refresh(current_serial: Option<String>, current_devices: Vec<String>) -> DeviceRefresh {
    let mut devices = Vec::new();
    // On timeout/failure keep the current device list instead of clearing it,
    // so a transient adb hiccup doesn't wipe the dropdown selection.
    let (devices_ok, diag) = match adb_output_bounded(&["devices"], REFRESH_ADB_TIMEOUT) {
        Some(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let diag = format!("adb devices OK, {} lines", text.lines().count());
            for line in text.lines().skip(1) {
                if let Some(serial) = line.split('\t').next() {
                    if !serial.is_empty() && line.contains("\tdevice") {
                        devices.push(serial.to_string());
                    }
                }
            }
            (true, diag)
        }
        Some(out) => (
            false,
            format!(
                "adb devices status={:?} stdout={:?}",
                out.status,
                String::from_utf8_lossy(&out.stdout)
            ),
        ),
        None => (false, "adb devices timed out (3s) or spawn failed".into()),
    };
    log!("[uiviewer] scan: {diag} -> devices={devices:?}");
    let target = current_serial
        .as_ref()
        .filter(|s| devices.iter().any(|d| d == *s))
        .cloned()
        .or_else(|| devices.first().cloned());
    let displays = if devices_ok {
        target.as_deref().map(get_displays)
    } else {
        None
    };
    DeviceRefresh {
        devices: if devices_ok { devices } else { current_devices },
        selected: target,
        displays,
        diag: Some(diag),
    }
}

fn http_request(
    addr: &str,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    read_timeout: std::time::Duration,
) -> Result<Vec<u8>, String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect: {e}"))?;
    // Bound reads so a silent peer cannot hang the background capture thread
    // indefinitely (the UI-side CAPTURE_TIMEOUT only abandons the wait).
    let _ = stream.set_read_timeout(Some(read_timeout));

    let head = match body {
        Some(b) => format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            b.len()
        ),
        None => format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"),
    };
    stream
        .write_all(head.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    if let Some(b) = body {
        stream.write_all(b).map_err(|e| format!("write body: {e}"))?;
    }

    let mut reader = std::io::BufReader::new(&mut stream);
    // Read status line
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|e| format!("read status: {e}"))?;
    let parts: Vec<&str> = status_line.split_whitespace().collect();
    let code = parts.get(1).unwrap_or(&"");

    let mut chunked = false;
    loop {
        let mut line = String::new();
        if reader
            .read_line(&mut line)
            .map_err(|e| format!("read header: {e}"))?
            == 0
        {
            break;
        }
        if line.to_lowercase().contains("transfer-encoding: chunked") {
            chunked = true;
        }
        if line.trim().is_empty() {
            break;
        }
    }

    let body = if chunked {
        read_chunked(&mut reader)?
    } else {
        let mut b = Vec::new();
        reader
            .read_to_end(&mut b)
            .map_err(|e| format!("read body: {e}"))?;
        b
    };

    if !code.starts_with('2') {
        let msg = String::from_utf8_lossy(&body).trim().to_string();
        let detail = if msg.is_empty() { String::new() } else { format!(": {msg}") };
        return Err(format!("HTTP {code}{detail}"));
    }

    Ok(body)
}

fn http_jsonrpc(method: &str, params: &[serde_json::Value]) -> Result<serde_json::Value, String> {
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let body = serde_json::to_vec(&payload).map_err(|e| format!("json encode: {e}"))?;
    let raw = http_request(U2V3_ADDR, "POST", "/jsonrpc/0", Some(&body), HTTP_READ_TIMEOUT)?;
    let data: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|e| format!("json decode: {e}"))?;
    if let Some(err) = data.get("error") {
        let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown");
        return Err(format!("{method}: {msg}"));
    }
    data.get("result")
        .cloned()
        .ok_or_else(|| format!("{method}: missing result"))
}

fn read_chunked(reader: &mut impl BufRead) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        reader
            .read_line(&mut size_line)
            .map_err(|e| format!("read chunk size: {e}"))?;
        let size = usize::from_str_radix(size_line.trim(), 16)
            .map_err(|e| format!("parse chunk size: {e}"))?;
        if size == 0 {
            break;
        }
        let mut chunk = vec![0u8; size];
        reader
            .read_exact(&mut chunk)
            .map_err(|e| format!("read chunk: {e}"))?;
        body.extend_from_slice(&chunk);
        // skip trailing CRLF after chunk
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf).map_err(|e| format!("read crlf: {e}"))?;
    }
    Ok(body)
}

fn stop_u2v3() {
    // Killing the adb client closes the streaming shell, which terminates
    // the remote app_process (mirrors Python's MockAdbProcess.kill()).
    if let Ok(mut guard) = U2V3_PROCESS.lock() {
        if let Some((_, mut child)) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// Only stops the u2.jar server if the stored child is still the one owned by
// `id`. A capture thread abandoned by the UI timeout (zombie) shares the global
// U2V3_PROCESS slot with a newer capture; without this check its failure path
// could kill the newer capture's freshly started server.
fn stop_u2v3_if_current(id: u64) {
    if let Ok(mut guard) = U2V3_PROCESS.lock() {
        if guard.as_ref().map(|(cur, _)| *cur) == Some(id) {
            if let Some((_, mut child)) = guard.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

// Release the u2.jar session owned by `serial` when the app switches away
// from it: kill the remote server via its streaming shell and drop our
// host-side port forward. Only sessions this app created are touched — a
// foreign tcp:9008 forward (another tool's) is left alone.
fn release_u2v3_resources(serial: &str) {
    let managed = U2V3_MANAGED_SERIALS
        .lock()
        .map(|g| g.iter().any(|s| s == serial))
        .unwrap_or(false);
    stop_u2v3();
    if managed {
        let _ = adb()
            .args(["-s", serial, "forward", "--remove", U2V3_FORWARD])
            .output();
    }
}

// True when the host-side forward still targets THIS device with OUR exact
// spec (`tcp:9008 -> tcp:9008`), local and remote columns both. Forwards are a
// host-global namespace, so with multiple devices connected a stale forward
// could route 127.0.0.1:9008 to the wrong phone — and a foreign tool could own
// a same-local-port forward pointing elsewhere — so the /ping probe below must
// never be trusted without this check.
fn u2v3_forward_matches(serial: &str) -> bool {
    let ok = adb()
        .args(["forward", "--list"])
        .output()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout).lines().any(|line| {
                let mut cols = line.split_whitespace();
                cols.next() == Some(serial)
                    && cols.next() == Some(U2V3_FORWARD)
                    && cols.next() == Some(U2V3_FORWARD)
            })
        })
        .unwrap_or(false);
    ok
}

// True when the u2.jar server answers /ping with "pong" (trimmed: some servers
// append a trailing newline/whitespace to the body).
fn u2v3_alive() -> bool {
    matches!(
        http_request(U2V3_ADDR, "GET", "/ping", None, U2V3_PING_TIMEOUT),
        Ok(b) if std::str::from_utf8(&b).map(|s| s.trim()) == Ok("pong")
    )
}

fn fnv1a_hash(bytes: &[u8]) -> u64 {
    let mut hash = FNV1A_OFFSET;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(FNV1A_PRIME);
    }
    hash
}

// Cheap keep-monitor probe for the U2 method: fetch only the hierarchy via
// JSON-RPC and return its hash. None on any failure (server dead, transport
// error, empty dump) — callers fall back to a full capture, which either
// recovers from a transient hiccup or stops monitoring via the normal
// failure policy.
fn probe_u2_hierarchy_hash() -> Option<u64> {
    let raw = http_jsonrpc(
        "dumpWindowHierarchy",
        &[U2V3_DUMP_COMPRESSED.into(), U2V3_DUMP_MAX_DEPTH.into()],
    )
        .and_then(|v| {
            v.as_str()
                .map(|s| s.as_bytes().to_vec())
                .ok_or_else(|| "dumpWindowHierarchy: not a string".into())
        })
        .ok()?;
    if raw.is_empty() {
        return None;
    }
    Some(fnv1a_hash(&raw))
}

// Reuse a healthy server across captures: the warm path costs a forward-ownership
// check plus one /ping probe, while the full cold start runs only when either
// fails.
fn ensure_u2v3_running(serial: &str) -> Result<(), String> {
    if u2v3_forward_matches(serial) && u2v3_alive() {
        return Ok(());
    }
    launch_u2v3(serial)
}

fn launch_u2v3(serial: &str) -> Result<(), String> {
    // Cold-start prerequisites live here (not in the capture fn) so the warm
    // reuse path in ensure_u2v3_running skips them entirely.

    // Require the u2.jar to be present (pushed via `python -m uiautomator2 init`).
    let jar_exists = adb()
        .args(["-s", serial, "shell", "ls", U2V3_JAR])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !jar_exists {
        return Err(format!(
            "{U2V3_JAR} not found on device — run 'python -m uiautomator2 init' to install"
        ));
    }

    // Drop stale forwards on serials THIS app previously set up (covers
    // switches that bypassed the UI-level release, e.g. the auto-selection
    // fallback when the active device disappears mid-capture). Foreign
    // forwards on other devices are left untouched. The extra remove below
    // covers an unmanaged target: taking over the device we are about to
    // capture is intentional, matching the unconditional pkill further down;
    // anything else surfaces later as an explicit "port may be in use" error.
    if let Ok(guard) = U2V3_MANAGED_SERIALS.lock() {
        for s in guard.iter() {
            let _ = adb()
                .args(["-s", s, "forward", "--remove", U2V3_FORWARD])
                .output();
        }
    }
    let _ = adb()
        .args(["-s", serial, "forward", "--remove", U2V3_FORWARD])
        .output();
    let status = adb()
        .args(["-s", serial, "forward", U2V3_FORWARD, U2V3_FORWARD])
        .output()
        .map_err(|e| format!("adb not found: {e}"))?;
    if !status.status.success() {
        return Err(format!("adb forward ({U2V3_FORWARD}) failed — port may be in use"));
    }
    // From here on the forward (and soon the server) on this serial is ours:
    // register it so every cleanup path knows it may touch this device.
    if let Ok(mut guard) = U2V3_MANAGED_SERIALS.lock() {
        if !guard.iter().any(|s| s == serial) {
            guard.push(serial.to_string());
        }
    }

    // Stop any previous uiautomator server process first.
    stop_u2v3();
    // Also clean up any orphaned server left by a previous run/exit. Match on the
    // main class, not the jar path: the CLASSPATH is an environment variable, so
    // `pkill -f u2.jar` never matches the app_process server's argv (verified).
    let _ = adb()
        .args(["-s", serial, "shell", &format!("pkill -f {U2V3_MAIN_CLASS} 2>/dev/null; true")])
        .output();
    std::thread::sleep(std::time::Duration::from_millis(300));

    // Start the uiautomator server (fresh each cold start) and wait for /ping.
    let cmd = format!(
        "CLASSPATH={U2V3_JAR} app_process / {U2V3_MAIN_CLASS} -p {U2V3_PORT}"
    );
    let mut child = adb()
        .args(["-s", serial, "shell", &cmd])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn uiautomator: {e}"))?;
    let my_id = U2V3_SERVER_ID.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut guard) = U2V3_PROCESS.lock() {
        *guard = Some((my_id, child));
    } else {
        let _ = child.kill();
        return Err("uiautomator process lock poisoned".into());
    }

    // Poll /ping until the server answers (see u2v3_alive for trimming note).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if u2v3_alive() {
            return Ok(());
        }
        std::thread::sleep(U2V3_PING_POLL_INTERVAL);
    }
    stop_u2v3_if_current(my_id);
    Err(format!("{U2V3_JAR} failed to start — server did not answer /ping in 30s"))
}

// Primary-display screenshot bytes via JSON-RPC takeScreenshot → base64 JPEG
// (MIME-style, newline every 76 chars — whitespace stripped before decoding),
// falling back to ADB screencap when the result is missing/undecodable.
// Secondary displays (logical id > 0) always use ADB screencap with the
// physical display token; "secondary" detection uses the logical id because
// the physical token is opaque for every display.
fn fetch_u2_screenshot(serial: &str, display_id: u32, display_physical: u64) -> Result<(Vec<u8>, ShotSource), String> {
    if display_id > 0 {
        let phys_str = display_physical.to_string();
        let out = adb()
            .args(["-s", serial, "exec-out", "screencap", "-p", "-d", &phys_str])
            .output()
            .map_err(|e| format!("screencap failed: {e}"))?;
        if !out.status.success() {
            return Err("screencap returned error".into());
        }
        return Ok((out.stdout, ShotSource::Adb));
    }
    use base64::Engine;
    let decoded = http_jsonrpc(
        "takeScreenshot",
        &[U2V3_PRIMARY_DISPLAY_ID.into(), U2V3_SCREENSHOT_QUALITY.into()],
    )
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .map(|b64: String| b64.chars().filter(|c| !c.is_whitespace()).collect::<String>())
        .filter(|b64| !b64.is_empty())
        .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(&b64).ok());
    match decoded {
        Some(bytes) => Ok((bytes, ShotSource::JsonRpc)),
        None => {
            let out = adb()
                .args(["-s", serial, "exec-out", "screencap", "-p"])
                .output()
                .map_err(|e| format!("screencap failed: {e}"))?;
            if !out.status.success() {
                return Err("screencap returned error".into());
            }
            Ok((out.stdout, ShotSource::Adb))
        }
    }
}

// Hierarchy dump via JSON-RPC dumpWindowHierarchy → plain XML string, retried
// a few times when the freshly launched server returns an empty hierarchy.
fn fetch_u2_hierarchy() -> Result<Vec<u8>, String> {
    // Retry a few times when the freshly launched server returns an empty
    // hierarchy; the inter-attempt delay runs between tries only, not after
    // the final failed one.
    const MAX_ATTEMPTS: usize = 3;
    for attempt in 0..MAX_ATTEMPTS {
        let raw = http_jsonrpc(
            "dumpWindowHierarchy",
            &[U2V3_DUMP_COMPRESSED.into(), U2V3_DUMP_MAX_DEPTH.into()],
        )
            .and_then(|v| {
                v.as_str()
                    .map(|s| s.as_bytes().to_vec())
                    .ok_or_else(|| "dumpWindowHierarchy: not a string".into())
            })?;
        if !raw.is_empty() {
            return Ok(raw);
        }
        if attempt + 1 < MAX_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(800));
        }
    }
    Err("dumpWindowHierarchy returned empty hierarchy".into())
}

fn uiautomator2_v3_capture(serial: &str, display_id: u32, display_physical: u64, png: PathBuf, xml: PathBuf) -> Result<(PathBuf, PathBuf, ShotSource), String> {
    // Warm server reuse: a single /ping probe; falls back to a full cold start
    // (jar probe + fresh forward + new app_process) only when it fails.
    ensure_u2v3_running(serial)?;

    let mut tmp_guard = TempGuard::new();

    // Screenshot and hierarchy come from independent channels (JSON-RPC over
    // the forward vs adb exec-out), so they are fetched concurrently. If the
    // u2 server serializes requests internally this merely degrades to the old
    // sequential timing — both calls are read-only, correctness is unaffected.
    let (shot_res, xml_res) = std::thread::scope(|s| {
        let h_shot = s.spawn(|| fetch_u2_screenshot(serial, display_id, display_physical));
        let h_xml = s.spawn(fetch_u2_hierarchy);
        (
            h_shot.join().unwrap_or_else(|_| Err("screenshot thread panicked".into())),
            h_xml.join().unwrap_or_else(|_| Err("hierarchy thread panicked".into())),
        )
    });

    let (png_bytes, shot_source) = shot_res?;
    tmp_guard.track(png.clone());
    std::fs::write(&png, &png_bytes).map_err(|e| format!("write png: {e}"))?;
    let xml_bytes = xml_res?;
    tmp_guard.track(xml.clone());
    std::fs::write(&xml, &xml_bytes).map_err(|e| format!("write xml: {e}"))?;

    tmp_guard.disarm();
    Ok((png, xml, shot_source))
}

impl App {
    fn load_screenshot(&mut self, path: &Path, ctx: &egui::Context) {
        let dir = path.parent().map(|p| p.to_path_buf());
        match load_texture(path, ctx) {
            Ok((tex, _size)) => {
                // Only discard the previous capture's temp file once the new image
                // actually loaded, so a failed load keeps the current screenshot usable.
                if let Some(p) = self.temp_screenshot.take() {
                    let _ = std::fs::remove_file(&p);
                }
                self.screenshot_texture = Some(tex);
                self.screenshot_path = Some(path.to_path_buf());
            }
            Err(e) => {
                log!("Failed to load screenshot: {e}");
            }
        }
        self.screenshot_dir = dir.clone();
        self.xml_dir = dir;
        // auto-load paired xml/uix from same directory
        let paired = path.with_extension("xml");
        let paired = if paired.exists() { paired } else { path.with_extension("uix") };
        if paired.exists() && self.xml_path.as_deref() != Some(paired.as_path()) {
            self.load_xml(&paired, ctx);
        }
    }

    fn load_xml(&mut self, path: &Path, ctx: &egui::Context) {
        let dir = path.parent().map(|p| p.to_path_buf());
        match std::fs::read_to_string(path) {
            Ok(content) => {
                // Only discard the previous capture's temp file once the new XML
                // actually loaded, so a failed load keeps the current XML usable.
                if let Some(p) = self.temp_xml.take() {
                    let _ = std::fs::remove_file(&p);
                }
                // Detect multi-display format: <displays>, or <hierarchy> with <display> children
                self.file_displays = get_display_ids_from_xml(&content);
                if !self.file_displays.is_empty() {
                    self.file_xml_content = Some(content.clone());
                    // Respect the current tree-display selection whenever it
                    // still exists in the freshly loaded/captured dump; only
                    // fall back to the first display when it does not.
                    if !self.file_displays.contains(&self.tree_display_id) {
                        self.tree_display_id = self.file_displays[0];
                    }
                    self.parsed_tree_display_id = self.tree_display_id;
                    self.root_node = parse_windows_xml(&content, self.tree_display_id);
                    if self.root_node.is_none() {
                        log!("Failed to parse --windows XML for display {}", self.tree_display_id);
                    }
                } else {
                    self.file_displays.clear();
                    self.file_xml_content = None;
                    self.root_node = parse_xml(&content);
                    if self.root_node.is_none() {
                        log!("Failed to parse XML — invalid format?");
                    }
                }
                self.xml_path = Some(path.to_path_buf());
            }
            Err(e) => {
                log!("Failed to read XML: {e}");
            }
        }
        self.xml_dir = dir.clone();
        self.screenshot_dir = dir;
        // auto-load paired screenshot from same directory
        if let Some(stem) = path.file_stem() {
            if let Some(dir) = path.parent() {
                for ext in ["png", "jpg", "jpeg"] {
                    let img_path = dir.join(stem).with_extension(ext);
                    if img_path.exists()
                        && self.screenshot_path.as_deref() != Some(img_path.as_path())
                    {
                        self.load_screenshot(&img_path, ctx);
                        break;
                    }
                }
            }
        }
    }

    fn save_files(&mut self) {
        let screenshot = match self.screenshot_path.clone() {
            Some(p) => p,
            None => {
                self.status_message = Some("No screenshot loaded".into());
                self.status_is_error = true;
                return;
            }
        };
        let xml = match self.xml_path.clone() {
            Some(p) => p,
            None => {
                self.status_message = Some("No XML loaded".into());
                self.status_is_error = true;
                return;
            }
        };

        let mut dialog = rfd::FileDialog::new().set_file_name("screenshot.png");
        if let Some(ref dir) = self.screenshot_dir {
            dialog = dialog.set_directory(dir);
        }
        if let Some(save_path) = dialog.save_file() {
            let dir = save_path.parent().unwrap_or(Path::new("."));
            let stem = save_path.file_stem().unwrap_or_default();
            let out_png = dir.join(stem).with_extension("png");
            let out_xml = dir.join(stem).with_extension("xml");

            // Re-encode the screenshot as PNG so the saved file matches its
            // extension even when the source is a JPEG (user-loaded or U2V3).
            let png_result = (|| -> Result<(), String> {
                let file = std::fs::File::open(&screenshot)
                    .map_err(|e| format!("open screenshot: {e}"))?;
                let img = image::ImageReader::new(std::io::BufReader::new(file))
                    .with_guessed_format()
                    .map_err(|e| format!("guess screenshot format: {e}"))?
                    .decode()
                    .map_err(|e| format!("decode screenshot: {e}"))?;
                img.save(&out_png).map_err(|e| format!("encode png: {e}"))
            })();
            let ok_png = png_result.is_ok();
            let ok_xml = std::fs::copy(&xml, &out_xml).is_ok();
            if ok_png && ok_xml {
                let name = stem.to_string_lossy();
                self.status_message = Some(format!("Saved as {name}.png / {name}.xml"));
                self.status_is_error = false;
            } else {
                self.status_message = Some("Failed to save files".into());
                self.status_is_error = true;
            }
        }
    }

    fn refresh_devices(&mut self, ctx: &egui::Context, force: bool) {
        let now = Instant::now();
        // Auto refresh is rate-limited and never overlaps an in-flight scan.
        // A forced (manual) refresh bypasses both so the user can re-scan at
        // any time, even while a previous scan is still running.
        if !force {
            if self.last_adb_check.is_some_and(|t| now - t < std::time::Duration::from_secs(15)) {
                return;
            }
            if self.refresh_rx.is_some() {
                return;
            }
        }
        log!(
            "[uiviewer] refresh_devices: force={force} last={:?}",
            self.last_adb_check
        );
        self.last_adb_check = Some(now);
        let current = self.selected_device.clone();
        let current_devices = self.adb_devices.clone();
        let (tx, rx) = mpsc::channel();
        let thread_ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = fetch_device_refresh(current, current_devices);
            if tx.send(result).is_ok() {
                thread_ctx.request_repaint();
            }
        });
        self.refresh_rx = Some(rx);
        self.refresh_start = Some(now);
    }

    fn poll_device_refresh(&mut self, ctx: &egui::Context) {
        let outcome = self.refresh_rx.as_ref().map(|rx| rx.try_recv());
        match outcome {
            Some(Ok(result)) => {
                self.refresh_rx = None;
                self.refresh_start = None;
                self.manual_refreshing = false;
                if let Some(d) = &result.diag {
                    self.status_message = Some(format!("[refresh] {d}"));
                    self.status_is_error = true;
                }
                self.adb_devices = result.devices;
                if !self.adb_devices.contains(&self.selected_device.as_deref().unwrap_or_default().to_string()) {
                    // Active device disappeared: fall back to the first one and
                    // release the gone device's u2.jar session. If a capture is
                    // still in flight for it, killing the server makes that
                    // capture fail fast instead of hanging to the 30s timeout
                    // (it was doomed anyway — its device is gone).
                    let old = self.selected_device.take();
                    self.selected_device = self.adb_devices.first().cloned();
                    if let Some(old) = old {
                        if self.selected_device.as_deref() != Some(&old) {
                            release_u2v3_resources(&old);
                        }
                    }
                }
                if self.selected_device == result.selected {
                    if let Some(ids) = result.displays {
                        if ids != self.available_displays {
                            self.available_displays = ids;
                            if !self.available_displays.iter().any(|(log, _)| *log == self.display_id) {
                                self.display_id = self.available_displays[0].0;
                            }
                        }
                    }
                }
                log!(
                    "[uiviewer] poll_device_refresh: devices={:?} selected={:?} displays={:?}",
                    self.adb_devices, self.selected_device, self.available_displays
                );
            }
            Some(Err(mpsc::TryRecvError::Empty)) => {
                if let Some(start) = self.refresh_start {
                    if start.elapsed() >= REFRESH_TIMEOUT {
                        // Abandon the hung refresh so it can't block future ones forever.
                        log!("[uiviewer] poll_device_refresh: refresh abandoned ({}s)", REFRESH_TIMEOUT.as_secs());
                        self.refresh_rx = None;
                        self.refresh_start = None;
                        self.manual_refreshing = false;
                        self.status_message =
                            Some("device refresh timed out — adb not responding".into());
                        self.status_is_error = true;
                    } else {
                        ctx.request_repaint_after(REFRESH_TIMEOUT - start.elapsed());
                    }
                }
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                log!("[uiviewer] poll_device_refresh: refresh thread disconnected");
                self.refresh_rx = None;
                self.refresh_start = None;
                self.manual_refreshing = false;
            }
            None => {}
        }
    }

    fn start_capture(&mut self, ctx: &egui::Context, method: CaptureMethod) {
        if self.capturing {
            log!("[uiviewer] start_capture: already capturing, ignored");
            return;
        }
        let serial = match self.selected_device.clone() {
            Some(s) => s,
            None => {
                log!("[uiviewer] start_capture: no device selected, aborting");
                self.status_message = Some("No device selected — connect a device and ensure it appears in the dropdown".into());
                self.status_is_error = true;
                // A capture that can't start is a capture failure: stop auto-
                // monitoring so the checkbox can't stay checked with nothing
                // scheduled (no tick, no probe) or respam errors every interval.
                self.stop_keep_monitor();
                return;
            }
        };

        log!(
            "[uiviewer] start_capture: method={:?} serial={serial} display_id={}",
            method, self.display_id
        );
        self.capturing = true;
        self.capture_start = Some(Instant::now());
        self.status_message = Some("Capturing...".into());
        self.status_is_error = false;

        // Take old temp paths out of tracking so a success/error can either
        // clean them or restore them independently of the fresh capture files.
        let old_png = self.temp_screenshot.take();
        let old_xml = self.temp_xml.take();

        let display_id = self.display_id;
        let phys_id = self.available_displays.iter()
            .find(|(log, _)| *log == display_id)
            .map(|(_, phys)| *phys)
            .unwrap_or(0);

        // Generate unique temp paths here (main thread) so App::drop can clean
        // up even when the capture thread is still running at exit.
        let id = next_temp_id();
        let tmp = std::env::temp_dir();
        let (png, xml) = match method {
            CaptureMethod::Adb => (
                tmp.join(format!("uiviewer_adb_screenshot_{id}.png")),
                tmp.join(format!("uiviewer_adb_dump_{id}.xml")),
            ),
            CaptureMethod::U2V3 => (
                tmp.join(format!("uiviewer_u2v3_screenshot_{id}.png")),
                tmp.join(format!("uiviewer_u2v3_dump_{id}.xml")),
            ),
        };
        self.in_flight_screenshot = Some(png.clone());
        self.in_flight_xml = Some(xml.clone());

        let (tx, rx) = mpsc::channel();
        // Record the serial on the main thread so exit cleanup can still find it
        // if the app closes while the capture thread is starting up.
        if method == CaptureMethod::U2V3 {
            if let Ok(mut guard) = U2V3_SERIAL.lock() {
                *guard = Some(serial.clone());
            }
        }

        self.capture_rx = Some(rx);
        self.pending_old_screenshot = old_png;
        self.pending_old_xml = old_xml;

        let thread_ctx = ctx.clone();
        std::thread::spawn(move || {
            let (method, result) = match method {
                CaptureMethod::U2V3 => {
                    (CaptureMethod::U2V3, uiautomator2_v3_capture(&serial, display_id, phys_id, png, xml))
                }
                CaptureMethod::Adb => {
                    stop_u2v3();
                    (
                        CaptureMethod::Adb,
                        adb_capture(&serial, display_id, phys_id, png.clone(), xml.clone())
                            .map(|(p, x)| (p, x, ShotSource::Adb)),
                    )
                }
            };
            match tx.send((method, result)) {
                Ok(_) => thread_ctx.request_repaint(),
                Err(mpsc::SendError((_, result))) => {
                    // Receiver dropped (app exited mid-capture): the fresh temps
                    // were disarmed from this thread's TempGuard, so remove them
                    // here to avoid leaving orphan files in the temp dir.
                    log!("[uiviewer] capture thread: receiver dropped");
                    if let Ok((png, xml, _)) = result {
                        let _ = std::fs::remove_file(&png);
                        let _ = std::fs::remove_file(&xml);
                    }
                }
            }
        });
    }

    // Turn keep-monitor off and clear its scheduling/probe state. Used on
    // capture failures so a dead device can't produce endless error spam,
    // and on manual uncheck.
    fn stop_keep_monitor(&mut self) {
        self.keep_monitor = false;
        self.next_auto_capture = None;
        self.monitor_probe_rx = None;
    }

    fn poll_capture(&mut self, ctx: &egui::Context) {
        let (method, result) = match self.capture_rx.as_ref() {
            None => return,
            Some(rx) => match rx.try_recv() {
                Ok((m, r)) => (m, r),
                Err(mpsc::TryRecvError::Empty) => {
                    if let Some(start) = self.capture_start {
                        let elapsed = start.elapsed();
                        if elapsed >= CAPTURE_TIMEOUT {
                            // Abandon the hung capture: dropping the receiver makes the
                            // thread's eventual send fail, so it removes its own fresh
                            // temp files (see SendError arm in start_capture). Also kill
                            // the u2.jar server so a zombie U2V3 thread's in-flight HTTP
                            // calls fail fast instead of hitting a newer capture's server.
                            log!("[uiviewer] poll_capture: capture timed out after {elapsed:?}");
                            stop_u2v3();
                            self.capture_rx = None;
                            self.capturing = false;
                            self.capture_start = None;
                            self.temp_screenshot = self.pending_old_screenshot.take();
                            self.temp_xml = self.pending_old_xml.take();
                            self.status_message = Some("capture timed out — device not responding".into());
                            self.status_is_error = true;
                            // A failed capture stops keep-monitor so a dead
                            // device can't produce endless error spam.
                            self.stop_keep_monitor();
                        } else {
                            // Wake up precisely at the deadline so the timeout fires
                            // even without user input (no always-on repaint loop).
                            ctx.request_repaint_after(CAPTURE_TIMEOUT - elapsed);
                        }
                    }
                    return;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Thread ended without sending (e.g. panic): kill any orphaned
                    // u2.jar server left behind by a U2V3 capture thread.
                    log!("[uiviewer] poll_capture: capture thread disconnected (panic?)");
                    stop_u2v3();
                    self.capture_rx = None;
                    self.capturing = false;
                    self.capture_start = None;
                    self.in_flight_screenshot = None;
                    self.in_flight_xml = None;
                    self.temp_screenshot = self.pending_old_screenshot.take();
                    self.temp_xml = self.pending_old_xml.take();
                    self.status_message = Some("capture failed: background thread ended unexpectedly".into());
                    self.status_is_error = true;
                    // Same policy as the timeout path: stop auto-monitoring.
                    self.stop_keep_monitor();
                    return;
                }
            },
        };

        self.capture_rx = None;
        self.capturing = false;
        self.capture_start = None;
        let old_png = self.pending_old_screenshot.take();
        let old_xml = self.pending_old_xml.take();
        let label = match method {
            CaptureMethod::Adb => "ADB",
            CaptureMethod::U2V3 => "u2 v3",
        };

        match result {
            Ok((png, xml, shot_source)) => {
                // The previous capture's unique temp files are no longer needed.
                if let Some(p) = old_png {
                    let _ = std::fs::remove_file(&p);
                }
                if let Some(p) = old_xml {
                    let _ = std::fs::remove_file(&p);
                }
                self.load_screenshot(&png, ctx);
                self.load_xml(&xml, ctx);
                // Refresh the keep-monitor change-detection baseline from this
                // fresh dump (also covers settle recaptures made during
                // monitoring), so the next probe compares against what is on
                // screen now.
                if self.keep_monitor {
                    if let Ok(bytes) = std::fs::read(&xml) {
                        self.monitor_xml_hash = Some(fnv1a_hash(&bytes));
                    }
                    self.monitor_probe_count = 0;
                }
                self.temp_screenshot = Some(png);
                self.temp_xml = Some(xml);
                self.in_flight_screenshot = None;
                self.in_flight_xml = None;
                // Surface the data source when U2V3's screenshot actually came
                // from adb screencap (secondary display, or RPC fallback).
                let via_adb = method == CaptureMethod::U2V3 && shot_source == ShotSource::Adb;
                let suffix = if via_adb { " (screenshot via ADB)" } else { "" };
                self.status_message = Some(match method {
                    CaptureMethod::U2V3 => format!("uiautomator2 v3 capture successful{suffix}"),
                    CaptureMethod::Adb => "ADB capture successful".into(),
                });
                self.status_is_error = false;
                // Keep-monitor: schedule the next automatic capture from this
                // completion (backpressure when a capture outlasts the interval).
                if self.keep_monitor {
                    self.next_auto_capture = Some(
                        Instant::now()
                            + std::time::Duration::from_secs_f64(self.monitor_interval_secs),
                    );
                }
            }
            Err(e) => {
                log!("[uiviewer] poll_capture: {label} capture FAILED: {e}");
                self.in_flight_screenshot = None;
                self.in_flight_xml = None;
                self.temp_screenshot = old_png;
                self.temp_xml = old_xml;
                self.status_message = Some(format!("{label} capture failed: {e}"));
                self.status_is_error = true;
                // Same policy as the timeout path: stop auto-monitoring.
                self.stop_keep_monitor();
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if !self.capturing {
            self.refresh_devices(&ctx, false);
        }
        self.poll_device_refresh(&ctx);
        self.poll_capture(&ctx);

        // Handle double-click tap — two-phase: tap now, capture after settle
        if let Some((x, y)) = self.pending_tap.take() {
            if let Some(ref serial) = self.selected_device.clone() {
                let mut input_args = vec![
                    "-s".to_string(),
                    serial.clone(),
                    "shell".to_string(),
                    "input".to_string(),
                ];
                if self.display_id > 0 {
                    input_args.push("--display".to_string());
                    input_args.push(self.display_id.to_string());
                }
                input_args.push("tap".to_string());
                input_args.push(format!("{:.0}", x));
                input_args.push(format!("{:.0}", y));
                let _ = adb()
                    .args(&input_args)
                    .output();
            }
            self.status_message = Some("Refreshing...".into());
            self.status_is_error = false;
            self.tap_settle_start = Some(Instant::now());
            ctx.request_repaint();
        }

        if let Some(tap_start) = self.tap_settle_start {
            if tap_start.elapsed() >= std::time::Duration::from_millis(800) {
                self.tap_settle_start = None;
                // Settle recaptures are user-driven feedback (tap/swipe result),
                // not monitor ticks.
                match self.last_capture {
                    Some(CaptureMethod::Adb) => self.start_capture(&ctx, CaptureMethod::Adb),
                    Some(CaptureMethod::U2V3) => self.start_capture(&ctx, CaptureMethod::U2V3),
                    None => self.start_capture(&ctx, CaptureMethod::Adb),
                }
            } else {
                ctx.request_repaint();
            }
        }

        // Keep-monitor: fire an automatic capture when due. Ticks yield to the
        // user's gesture chain (tap/swipe settle recaptures) so the feedback
        // capture is never displaced; poll_capture reschedules the next tick
        // from each completion, which also provides backpressure when a
        // capture takes longer than the interval.
        if self.keep_monitor {
            // Liveness heartbeat: egui only repaints on input or explicit
            // requests; the tick/probe/capture wake-up chains below have many
            // branches, so guarantee a periodic frame regardless.
            ctx.request_repaint_after(std::time::Duration::from_millis(MONITOR_HEARTBEAT_MS));
            // A background hierarchy probe is in flight (U2 two-tier mode):
            // consume its result without blocking the UI thread.
            if let Some(rx) = self.monitor_probe_rx.take() {
                match rx.try_recv() {
                    Err(mpsc::TryRecvError::Empty) => {
                        self.monitor_probe_rx = Some(rx);
                        ctx.request_repaint_after(std::time::Duration::from_millis(50));
                    }
                    result => {
                        // Probe thread died → None → fall back to a full capture.
                        let hash = match result {
                            Ok(h) => h,
                            _ => None,
                        };
                        if !self.capturing
                            && self.pending_tap.is_none()
                            && self.tap_settle_start.is_none()
                        {
                            let mut fire = true;
                            if let Some(h) = hash {
                                self.monitor_probe_count += 1;
                                let forced =
                                    self.monitor_probe_count >= MONITOR_FORCE_REFRESH_EVERY;
                                if forced {
                                    self.monitor_probe_count = 0;
                                }
                                fire = forced || self.monitor_xml_hash != Some(h);
                                self.monitor_xml_hash = Some(h);
                            }
                            if fire {
                                self.next_auto_capture = None;
                                let method = self.last_capture.unwrap_or(CaptureMethod::Adb);
                                self.start_capture(&ctx, method);
                            } else {
                                // Unchanged: schedule the next probe from now.
                                self.next_auto_capture = Some(
                                    Instant::now()
                                        + std::time::Duration::from_secs_f64(
                                            self.monitor_interval_secs,
                                        ),
                                );
                                ctx.request_repaint_after(std::time::Duration::from_secs_f64(
                                    self.monitor_interval_secs,
                                ));
                            }
                        }
                        // Gesture/capture started while probing: skip this
                        // round. Reschedule defensively so the monitor can't
                        // stall even if the blocking action ends without
                        // passing through poll_capture (e.g. no-device bail).
                        self.next_auto_capture = Some(
                            Instant::now()
                                + std::time::Duration::from_secs_f64(self.monitor_interval_secs),
                        );
                    }
                }
            } else if let Some(next) = self.next_auto_capture {
                let now = Instant::now();
                if now >= next {
                    if !self.capturing
                        && self.pending_tap.is_none()
                        && self.tap_settle_start.is_none()
                    {
                        let method = self.last_capture.unwrap_or(CaptureMethod::Adb);
                        if method == CaptureMethod::U2V3 {
                            // Two-tier probing: fetch only the hierarchy off
                            // the UI thread and decide on the next frame. An
                            // adb-method probe would cost the same as a full
                            // capture (JVM spawn), so it always captures fully.
                            let (tx, rx) = mpsc::channel();
                            self.monitor_probe_rx = Some(rx);
                            std::thread::spawn(move || {
                                let _ = tx.send(probe_u2_hierarchy_hash());
                            });
                        } else {
                            self.next_auto_capture = None;
                            self.start_capture(&ctx, method);
                        }
                    }
                    // Blocked by an in-flight gesture/capture: retry on the
                    // next repaint (those paths already request repaints).
                } else {
                    ctx.request_repaint_after(next - now);
                }
            }
        }

        // auto-expand & scroll tree (only scroll when image-click requested)
        let selection_changed = self.selected_path != self.last_selected
            && self.selected_path.is_some();
        if selection_changed {
            self.last_selected = self.selected_path.clone();
            if let Some(ref path) = self.selected_path {
                for i in 0..=path.len() {
                    self.expanded.insert(path[..i].to_vec());
                }
            }
        }

        // Re-parse when display selector changes for multi-display file
        if self.file_displays.len() > 1 && self.tree_display_id != self.parsed_tree_display_id {
            self.parsed_tree_display_id = self.tree_display_id;
            if let Some(content) = self.file_xml_content.as_deref() {
                self.root_node = parse_windows_xml(content, self.tree_display_id);
                self.selected_path = None;
                self.expanded.clear();
            }
        }

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                let load_screenshot = || {
                    let mut dialog = rfd::FileDialog::new()
                        .add_filter("Images", &["png", "jpg", "jpeg"]);
                    if let Some(ref dir) = self.screenshot_dir {
                        dialog = dialog.set_directory(dir);
                        let _ = std::env::set_current_dir(dir);
                    }
                    dialog.pick_file()
                };
                if ui.button("📷 Load Screenshot").clicked() {
                    if let Some(path) = load_screenshot() {
                        self.load_screenshot(&path, &ctx);
                    }
                }
                let load_xml = || {
                    let mut dialog = rfd::FileDialog::new()
                        .add_filter("XML/UIX", &["xml", "uix"]);
                    if let Some(ref dir) = self.xml_dir {
                        dialog = dialog.set_directory(dir);
                        let _ = std::env::set_current_dir(dir);
                    }
                    dialog.pick_file()
                };
                if ui.button("📄 Load XML").clicked() {
                    if let Some(path) = load_xml() {
                        self.load_xml(&path, &ctx);
                    }
                }
                if ui.add_enabled(!self.manual_refreshing, egui::Button::new("🔄 Refresh device")).clicked() {
                    // Force: abandon any in-flight scan (dropping the receiver
                    // makes its send fail harmlessly; the thread self-cleans)
                    // and bypass the rate limit so every click scans now.
                    self.manual_refreshing = true;
                    self.refresh_rx = None;
                    self.refresh_start = None;
                    self.refresh_devices(&ctx, true);
                }
                if !self.adb_devices.is_empty() {
                    let current = self.selected_device.clone().unwrap_or_default();
                    let previous = self.selected_device.clone();
                    ui.add_enabled_ui(!self.capturing, |ui| {
                        egui::ComboBox::from_id_salt("device_selector")
                            .selected_text(current.as_str())
                            .width(160.0)
                            .show_ui(ui, |ui| {
                                for serial in &self.adb_devices {
                                    ui.selectable_value(
                                        &mut self.selected_device,
                                        Some(serial.clone()),
                                        serial,
                                    );
                                }
                            });
                    });
                    // Manual device switch: free the previous device's u2.jar
                    // session (server + host forward) so the new device can
                    // bind tcp:9008 cleanly. The selector is disabled while a
                    // capture is in flight, so this never kills an active one.
                    if self.selected_device != previous {
                        if let Some(old) = previous {
                            release_u2v3_resources(&old);
                        }
                    }
                }
                if !self.adb_devices.is_empty() {
                    let current_disp = format!("Disp {}", self.display_id);
                    let prev_disp = self.display_id;
                    ui.add_enabled_ui(!self.capturing, |ui| {
                        egui::ComboBox::from_id_salt("display_selector")
                            .selected_text(&current_disp)
                            .width(70.0)
                            .show_ui(ui, |ui| {
                                for &(log, _phys) in &self.available_displays {
                                    let label = format!("Disp {log}");
                                    ui.selectable_value(&mut self.display_id, log, label);
                                }
                            });
                    });
                    // Re-selecting the top display always moves the tree along.
                    if self.display_id != prev_disp {
                        self.tree_display_id = self.display_id;
                    }
                }
                ui.separator();
                // Keep-monitor switch: auto-capture with the last used method
                // at a fixed interval. Toggling on fires immediately; the two
                // capture buttons are disabled while it runs.
                if ui.checkbox(&mut self.keep_monitor, "Keep monitor").changed() {
                    if self.keep_monitor {
                        // Fire immediately, then reschedule from completions.
                        self.next_auto_capture = Some(Instant::now());
                    } else {
                        self.stop_keep_monitor();
                    }
                }
                // Interval stepper: [-]/[+] adjust in 0.5s steps (clamped to
                // 0.5–60s); changes take effect immediately by rescheduling the
                // next tick from now.
                let mut step = 0.0f64;
                ui.add_enabled_ui(self.keep_monitor, |ui| {
                    if ui.small_button("-").clicked() {
                        step = -0.5;
                    }
                    ui.monospace(format!("{:>4.1}s", self.monitor_interval_secs))
                        .on_hover_text("Auto-capture interval in seconds (-/+ adjust by 0.5)");
                    if ui.small_button("+").clicked() {
                        step = 0.5;
                    }
                });
                if step != 0.0 {
                    self.monitor_interval_secs =
                        (self.monitor_interval_secs + step).clamp(0.5, 60.0);
                    if !self.capturing && self.monitor_probe_rx.is_none() {
                        self.next_auto_capture = Some(
                            Instant::now()
                                + std::time::Duration::from_secs_f64(self.monitor_interval_secs),
                        );
                    }
                }
                if ui.add_enabled(!self.capturing && !self.keep_monitor, egui::Button::new("📱 ADB Capture")).clicked() {
                    self.last_capture = Some(CaptureMethod::Adb);
                    self.start_capture(&ctx, CaptureMethod::Adb);
                }
                if ui.add_enabled(!self.capturing && !self.keep_monitor, egui::Button::new("⚡ u2 Capture")).clicked() {
                    self.last_capture = Some(CaptureMethod::U2V3);
                    self.start_capture(&ctx, CaptureMethod::U2V3);
                }
                if ui.add_enabled(!self.capturing, egui::Button::new("💾 Save")).clicked() {
                    self.save_files();
                }
                ui.separator();
                if let Some(path) = &self.screenshot_path {
                    ui.label(format!(
                        "Screenshot: {}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ));
                }
                if let Some(path) = &self.xml_path {
                    ui.label(format!(
                        "XML: {}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Clear").clicked() {
                        self.selected_path = None;
                    }
                });
            });
        });

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                if let Some(msg) = &self.status_message {
                    let color = if self.status_is_error {
                        Color32::RED
                    } else {
                        Color32::from_rgb(0, 180, 0)
                    };
                    ui.colored_label(color, msg);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(p) = self.click_pos {
                        ui.label(format!("Click: ({:.0}, {:.0})", p.x, p.y));
                    }
                });
            });
        });

        egui::Panel::right("properties_panel")
            .resizable(true)
            .default_size(self.properties_width)
            .min_size(80.0)
            .show(ui, |ui| {
            self.properties_width = ui.max_rect().width();
            ui.heading("Node Tree");
            if !self.file_displays.is_empty() || self.root_node.is_some() {
                let current_disp = format!("Display {}", self.tree_display_id);
                let ids: Vec<u32> = if !self.file_displays.is_empty() {
                    self.file_displays.clone()
                } else {
                    self.available_displays.iter().map(|(log, _)| *log).collect()
                };
                egui::ComboBox::from_id_salt("tree_display_selector")
                    .selected_text(&current_disp)
                    .width(140.0)
                    .show_ui(ui, |ui| {
                        for &log in &ids {
                            let label = format!("Display {log}");
                            ui.selectable_value(&mut self.tree_display_id, log, label);
                        }
                    });
            }
            ui.separator();

            let tree_max = (ui.available_height() * 0.45).max(100.0);
            let mut expanded = std::mem::take(&mut self.expanded);
            egui::ScrollArea::vertical()
                .id_salt("tree_scroll")
                .max_height(tree_max)
                .auto_shrink([true, false])
                .hscroll(true)
                .show(ui, |ui| {
                    let mut node_rects = Vec::new();
                    if let Some(ref root) = self.root_node {
                        let sel = self.selected_path.as_deref();
                        let hov = self.hovered_path.as_deref();
                        render_tree(ui, root, &[], &mut expanded, sel, hov, &mut node_rects);
                    } else {
                        ui.weak("No XML loaded");
                    }
                    let mut click_info: Option<Pos2> = None;
                    ui.input(|i| {
                        if i.pointer.any_click() {
                            click_info = i.pointer.interact_pos();
                        }
                    });
                    if let Some(pos) = click_info {
                        // Only honor clicks inside the tree's visible viewport.
                        // Scrolled-out rows still have live screen-space rects
                        // that extend past the viewport bottom into the
                        // Properties area below — without this gate a click
                        // there would phantom-select a hidden node.
                        if ui.clip_rect().contains(pos) {
                            for (path, rect) in &node_rects {
                                if rect.contains(pos) {
                                    self.selected_path = Some(path.clone());
                                    self.last_selected = Some(path.clone());
                                    break;
                                }
                            }
                        }
                    }
                    if let Some(ref target_path) = self.scroll_to_target {
                        for (path, rect) in &node_rects {
                            if path == target_path {
                                ui.scroll_to_rect(*rect, Some(egui::Align::Center));
                                break;
                            }
                        }
                        self.scroll_to_target = None;
                    }
                });
            self.expanded = expanded;

            ui.separator();
            ui.heading("Properties");
            ui.separator();

            let target = self
                .hovered_path
                .as_ref()
                .or(self.selected_path.as_ref());
            let node = target
                .and_then(|p| self.root_node.as_ref()?.node_at(p));

            if let Some(node) = node {
                ui.colored_label(
                    egui::Color32::BLACK,
                    format!(
                        "Bounds: [{:.0},{:.0}][{:.0},{:.0}]",
                        node.bounds.min.x,
                        node.bounds.min.y,
                        node.bounds.max.x,
                        node.bounds.max.y,
                    ),
                );
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt("props_scroll")
                    .show(ui, |ui| {
                        for (key, value) in &node.attrs {
                            if key == "bounds" {
                                continue;
                            }
                            ui.label(
                                egui::RichText::new(format!("{key}:"))
                                    .strong(),
                            );
                            ui.add(
                                egui::Label::new(value)
                                    .wrap()
                                    .selectable(true),
                            );
                        }
                    });

                if self.hovered_path.is_some()
                    && self.hovered_path != self.selected_path
                {
                    ui.separator();
                    ui.label(
                        egui::RichText::new("Click to select this element")
                            .color(egui::Color32::GRAY)
                            .italics(),
                    );
                }
            } else {
                ui.weak("Hover over an element to inspect");
            }
        });

        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(ref tex) = self.screenshot_texture {
                let img_size: Vec2 = tex.size_vec2();
                let available = ui.available_size();
                let scale = (available.x / img_size.x).min(available.y / img_size.y);
                let scaled = img_size * scale;
                let offset = Vec2::new(
                    (available.x - scaled.x).max(0.0) / 2.0,
                    (available.y - scaled.y).max(0.0) / 2.0,
                );

                let origin = ui.max_rect().min;
                let image_rect = Rect::from_min_size(origin + offset, scaled);

                ui.painter().image(
                    tex.id(),
                    image_rect,
                    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );

                let response =
                    ui.interact(image_rect, ui.next_auto_id(), Sense::click_and_drag());

                let img_pos_from = |mouse: Pos2| -> Pos2 {
                    pos2(
                        (mouse.x - image_rect.min.x) / scale,
                        (mouse.y - image_rect.min.y) / scale,
                    )
                };

                if response.drag_started() {
                    if let Some(mouse) = response.hover_pos() {
                        self.drag_start_img = Some(img_pos_from(mouse));
                    }
                }

                if response.dragged() {
                    if let Some(pos) = ui.ctx().input(|i| i.pointer.latest_pos()) {
                        self.last_hover_img_pos = Some(img_pos_from(pos));
                    }
                }

                if response.drag_stopped() {
                    if let Some(start) = self.drag_start_img.take() {
                        let end = self.last_hover_img_pos.unwrap_or(start);
                        if (end - start).length() >= 10.0 && self.selected_device.is_some() {
                            if let Some(ref serial) = self.selected_device.clone() {
                                let mut args = vec![
                                    "-s".to_string(),
                                    serial.clone(),
                                    "shell".to_string(),
                                    "input".to_string(),
                                ];
                                if self.display_id > 0 {
                                    args.push("--display".to_string());
                                    args.push(self.display_id.to_string());
                                }
                                args.push("swipe".to_string());
                                args.push(format!("{:.0}", start.x));
                                args.push(format!("{:.0}", start.y));
                                args.push(format!("{:.0}", end.x));
                                args.push(format!("{:.0}", end.y));
                                let _ = adb().args(&args).output();
                            }
                            self.tap_settle_start = Some(Instant::now());
                            ctx.request_repaint();
                            self.status_message = Some("Refreshing...".into());
                            self.status_is_error = false;
                        }
                    }
                }

                if let Some(mouse) = response.hover_pos() {
                    let img_pos = img_pos_from(mouse);
                    let moved = self.last_hover_img_pos.is_none_or(|last| (img_pos - last).length() >= 5.0);
                    if moved && !response.dragged() {
                        self.hovered_path = self
                            .root_node
                            .as_ref()
                            .and_then(|root| root.find_branch(img_pos, &mut Vec::new()));
                    }
                    if !response.dragged() {
                        self.last_hover_img_pos = Some(img_pos);
                    }

                    if response.clicked() {
                        let same_spot = self.click_pos.is_some_and(|last| (img_pos - last).length() < 10.0);
                        if same_spot {
                            if let Some(cur) = self.selected_path.clone() {
                                if cur.len() > 1 {
                                    let parent = cur[..cur.len() - 1].to_vec();
                                    self.selected_path = Some(parent.clone());
                                    self.hovered_path = Some(parent.clone());
                                    self.scroll_to_target = Some(parent);
                                }
                            }
                        } else {
                            self.selected_path = self.hovered_path.clone();
                            self.scroll_to_target = self.hovered_path.clone();
                        }
                        self.click_pos = Some(img_pos);
                    }
                    if response.double_clicked() && self.selected_device.is_some() {
                        self.pending_tap = Some((img_pos.x, img_pos.y));
                    }
                } else if !response.dragged() {
                    self.hovered_path = None;
                    self.last_hover_img_pos = None;
                }

                let draw_highlight = |path: &[usize], color: Color32, stroke_width: f32| {
                    if let Some(node) = self.root_node.as_ref().and_then(|r| r.node_at(path)) {
                        let r = Rect::from_min_size(
                            image_rect.min + node.bounds.min.to_vec2() * scale,
                            node.bounds.size() * scale,
                        );
                        ui.painter().rect_stroke(r, 0.0, Stroke::new(stroke_width, color), egui::StrokeKind::Middle);
                    }
                };

                if let Some(ref path) = self.hovered_path {
                    draw_highlight(path, Color32::RED, 2.0);
                }
                if let Some(ref path) = self.selected_path {
                    if Some(path) != self.hovered_path.as_ref() {
                        draw_highlight(path, Color32::from_rgb(0, 150, 0), 3.0);
                    }
                }
            } else {
                ui.vertical_centered(|ui| {
                    ui.add_space(120.0);
                    ui.heading("UI Viewer");
                    ui.label("Load a screenshot and its uiautomator XML dump");
                    ui.label("to inspect UI elements by hovering on the image.");
                });
            }
        });
    }
}

fn main() -> eframe::Result<()> {
    #[cfg(target_os = "windows")]
    {
        extern "system" {
            fn FreeConsole() -> i32;
        }
        unsafe { FreeConsole(); }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_decorations(true)
            .with_inner_size([1280.0, 800.0]),
        centered: true,
        ..Default::default()
    };
    let result = eframe::run_native("UI Viewer", options, Box::new(|cc| {
        cc.egui_ctx.set_theme(egui::Theme::Light);
        let mut fonts = egui::FontDefinitions::default();
        #[cfg(target_os = "windows")]
        let cjk_paths = [
            "C:\\Windows\\Fonts\\msyh.ttc",
            "C:\\Windows\\Fonts\\msyhbd.ttc",
            "C:\\Windows\\Fonts\\simsun.ttc",
            "C:\\Windows\\Fonts\\SIMKAI.ttf",
            "C:\\Windows\\Fonts\\deng.ttf",
        ];
        #[cfg(not(target_os = "windows"))]
        let cjk_paths = [
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Medium.ttc",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        ];
        for path in &cjk_paths {
            if let Ok(data) = std::fs::read(path) {
                fonts.font_data.insert("cjk".to_owned(), egui::FontData::from_owned(data).into());
                for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                    fonts.families.entry(family).or_default().push("cjk".to_owned());
                }
                break;
            }
        }
        cc.egui_ctx.set_fonts(fonts);
        Ok(Box::new(App::default()))
    }));

    // Cleanup: kill the uiautomator app_process, remove forward — but only
    // for serials whose session THIS app created. If we merely reused a server
    // started by another tool (python-uiautomator2), its forward and server
    // belong to that tool and must survive our exit.
    stop_u2v3();
    if let Ok(guard) = U2V3_SERIAL.lock() {
        if let Some(ref serial) = *guard {
            let managed = U2V3_MANAGED_SERIALS
                .lock()
                .map(|m| m.iter().any(|s| s == serial))
                .unwrap_or(false);
            if managed {
                let _ = adb()
                    .args(["-s", serial, "shell", &format!("pkill -f {U2V3_MAIN_CLASS} 2>/dev/null; true")])
                    .output();
                let _ = adb()
                    .args(["-s", serial, "forward", "--remove", U2V3_FORWARD])
                    .output();
            }
        }
    }

    result
}
