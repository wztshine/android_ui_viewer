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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

static U2_STARTED: AtomicBool = AtomicBool::new(false);
static U2_SERIAL: Mutex<Option<String>> = Mutex::new(None);

const U2_ADDR: &str = "127.0.0.1:7912";
const U2_FORWARD: &str = "tcp:7912";
const ATX_AGENT: &str = "/data/local/tmp/atx-agent";
const UIAUTOMATOR_APK: &str = "/data/local/tmp/app-uiautomator.apk";
const DEVICE_DUMP: &str = "/sdcard/uiviewer_dump.xml";

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
}

fn adb() -> Command {
    let mut cmd = Command::new("adb");
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
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
                if area < best_area {
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

    fn get_attr(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

fn tree_label(node: &UiNode) -> String {
    let cls = node
        .get_attr("class")
        .map(|c| c.rsplit('.').next().unwrap_or(c))
        .unwrap_or("?");
    let text = node.get_attr("text").filter(|v| !v.is_empty() && *v != "null");
    let rid = node.get_attr("resource-id").filter(|v| !v.is_empty() && *v != "null");
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
        s.push_str("…");
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

        let label = tree_label(node);
        let rich = if is_selected {
            egui::RichText::new(&label).color(Color32::from_rgb(0, 150, 0))
        } else if is_hovered {
            egui::RichText::new(&label).color(Color32::RED)
        } else {
            egui::RichText::new(&label)
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
    last_tree_display_id: u32,
    pending_tap: Option<(f32, f32)>,
    tap_settle_start: Option<Instant>,
    last_capture: Option<CaptureMethod>,
    click_pos: Option<Pos2>,
    last_hover_img_pos: Option<Pos2>,
    file_displays: Vec<u32>,
    file_xml_content: Option<String>,
    properties_width: f32,
    drag_start_img: Option<Pos2>,
    last_adb_check: Option<Instant>,
}

#[derive(Clone, PartialEq)]
enum CaptureMethod {
    Adb,
    U2,
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
            last_tree_display_id: 0,
            pending_tap: None,
            tap_settle_start: None,
            last_capture: None,
            click_pos: None,
            last_hover_img_pos: None,
            file_displays: Vec::new(),
            file_xml_content: None,
            properties_width: 350.0,
            drag_start_img: None,
            last_adb_check: None,
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(p) = self.temp_screenshot.take() {
            let _ = std::fs::remove_file(&p);
        }
        if let Some(p) = self.temp_xml.take() {
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

    let children: Vec<UiNode> = node
        .children()
        .filter(|c| c.is_element())
        .filter_map(|c| parse_node(&c))
        .collect();

    Some(UiNode { bounds, attrs, children })
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
    Some(UiNode {
        bounds,
        attrs: vec![("class".into(), "android.widget.FrameLayout".into())],
        children: nodes,
    })
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
            root.children().filter(|c| c.is_element() && c.tag_name().name() == "display")
                .find_map(|c| {
                    let id = c.attribute("id")?.parse::<u32>().ok()?;
                    if id == display_id { Some(c) } else { None }
                })
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
    let img = image::ImageReader::open(path)
        .map_err(|e| format!("open image: {e}"))?
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
        // Check for <displays> wrapper first
        if let Some(displays) = root.children().find(|c| c.is_element() && c.tag_name().name() == "displays") {
            return displays.children()
                .filter(|c| c.is_element() && c.tag_name().name() == "display")
                .filter_map(|c| c.attribute("id")?.parse::<u32>().ok())
                .collect();
        }
        // Fallback: <display> directly under <hierarchy>
        return root.children()
            .filter(|c| c.is_element() && c.tag_name().name() == "display")
            .filter_map(|c| c.attribute("id")?.parse::<u32>().ok())
            .collect();
    }
    Vec::new()
}

fn adb_capture(serial: &str, display_logical: u32, display_physical: u64) -> Result<(PathBuf, PathBuf), String> {
    let tmp = std::env::temp_dir();
    let suffix = if display_logical > 0 { format!("_d{display_logical}") } else { String::new() };
    let png = tmp.join(format!("uiviewer_adb_screenshot{suffix}.png"));
    let xml = tmp.join(format!("uiviewer_adb_dump{suffix}.xml"));
    let mut tmp_guard = TempGuard::new();

    let img = if display_physical > 0 {
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
    if !img.status.success() {
        return Err("screencap returned error".into());
    }
    std::fs::write(&png, &img.stdout).map_err(|e| format!("write png: {e}"))?;
    tmp_guard.track(png.clone());

    // Always try --windows first so U2 and ADB capture see the same window coverage.
    // Without --windows, uiautomator dump only returns the active window root,
    // while U2's dumpWindowHierarchy iterates getWindowRoots() (all windows).
    // Fall back to non-windows dump if --windows is not supported (older Android).
    let dump = adb()
        .args(["-s", serial, "shell", "uiautomator", "dump", "--windows", DEVICE_DUMP])
        .output()
        .map_err(|e| format!("uiautomator dump failed: {e}"))?;
    if !dump.status.success() {
        let fallback = adb()
            .args(["-s", serial, "shell", "uiautomator", "dump", DEVICE_DUMP])
            .output()
            .map_err(|e| format!("uiautomator dump failed (fallback): {e}"))?;
        if !fallback.status.success() {
            return Err("uiautomator dump returned error (both --windows and fallback)".into());
        }
    }

    let pull = adb()
        .args(["-s", serial, "pull", DEVICE_DUMP, &xml.to_string_lossy().to_string()])
        .output()
        .map_err(|e| format!("adb pull failed: {e}"))?;

    // always clean up temp files on device
    let _ = adb()
        .args(["-s", serial, "shell", "rm", DEVICE_DUMP])
        .output();

    if !pull.status.success() {
        return Err("adb pull returned error".into());
    }
    tmp_guard.track(xml.clone());

    tmp_guard.disarm();
    Ok((png, xml))
}

fn get_displays(serial: &str) -> Vec<(u32, u64)> {
    // Get physical display IDs from SurfaceFlinger
    let physical_output = adb()
        .args(["-s", serial, "shell", "dumpsys", "SurfaceFlinger", "--display-id"])
        .output()
        .ok()
        .filter(|o| o.status.success());
    let mut logical_to_physical: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
    if let Some(output) = physical_output {
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
    let logical_output = adb()
        .args(["-s", serial, "shell", "dumpsys", "display"])
        .output()
        .ok()
        .filter(|o| o.status.success());
    let mut logical_ids: Vec<u32> = if let Some(output) = logical_output {
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
        return vec![(0, 0)];
    }
    logical_ids.into_iter().map(|log| {
        let phys = logical_to_physical.get(&log).copied().unwrap_or(0);
        (log, phys)
    }).collect()
}

fn http_get(path: &str) -> Result<Vec<u8>, String> {
    let mut stream =
        TcpStream::connect(U2_ADDR).map_err(|e| format!("connect: {e}"))?;

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {U2_ADDR}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write: {e}"))?;

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

fn extract_xml(raw: &[u8]) -> Vec<u8> {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(raw) {
        if let Some(s) = v.get("result").and_then(|v| v.as_str()) {
            return s.as_bytes().to_vec();
        }
    }
    raw.to_vec()
}

fn uiautomator2_capture(serial: &str, display_id: u32) -> Result<(PathBuf, PathBuf), String> {
    let tmp = std::env::temp_dir();
    let suffix = if display_id > 0 { format!("_d{display_id}") } else { String::new() };
    let png = tmp.join(format!("uiviewer_u2_screenshot{suffix}.png"));
    let xml = tmp.join(format!("uiviewer_u2_dump{suffix}.xml"));
    let mut tmp_guard = TempGuard::new();

    // Check if atx-agent is already running — if so, reuse it
    if !U2_STARTED.load(Ordering::Relaxed) {
        let already_running = adb()
            .args(["-s", serial, "shell", "pidof", "atx-agent"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| {
                let s = String::from_utf8_lossy(&o.stdout);
                Some(s.split_whitespace().next()?.parse::<u32>().ok()?)
            })
            .is_some();

        if !already_running {
            let _ = adb()
                .args(["-s", serial, "shell", "kill -9 $(pidof atx-agent) 2>/dev/null; true"])
                .output();
            // Kill stale uiautomator process so atx-agent starts fresh
            let _ = adb()
                .args(["-s", serial, "shell", "am force-stop com.github.uiautomator"])
                .output();
            // Start atx-agent server in daemon mode (-d is a flag of the server subcommand)
            let _ = adb()
                .args(["-s", serial, "shell", ATX_AGENT, "server", "-d"])
                .output();
            std::thread::sleep(std::time::Duration::from_millis(2000));

            // Verify atx-agent started
            let ok = adb()
                .args(["-s", serial, "shell", "pidof", "atx-agent"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| {
                    let s = String::from_utf8_lossy(&o.stdout);
                    Some(s.split_whitespace().next()?.parse::<u32>().ok()?)
                })
                .is_some();
            if !ok {
                return Err(format!("{ATX_AGENT} failed to start — check if binary exists and is executable"));
            }
        }
        U2_STARTED.store(true, Ordering::Relaxed);
        // Record serial early so exit cleanup can kill atx-agent/forward even if capture fails
        if let Ok(mut guard) = U2_SERIAL.lock() {
            *guard = Some(serial.to_string());
        }
    }

    // Ensure uiautomator test APK is installed (check every time)
    let pkgs = adb()
        .args(["-s", serial, "shell", "pm", "list", "packages"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let apk_installed = pkgs.lines().any(|l| l.trim() == "package:com.github.uiautomator");
    if !apk_installed {
        let apk_file = adb()
            .args(["-s", serial, "shell", "ls", UIAUTOMATOR_APK])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if apk_file {
            let install = adb()
                .args(["-s", serial, "shell", "pm", "install", "-r", UIAUTOMATOR_APK])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !install {
                return Err("installing app-uiautomator.apk failed".into());
            }
            // Restart atx-agent so it detects the newly installed service
            let _ = adb()
                .args(["-s", serial, "shell", "kill -9 $(pidof atx-agent) 2>/dev/null; true"])
                .output();
            let _ = adb()
                .args(["-s", serial, "shell", ATX_AGENT, "server", "-d"])
                .output();
            std::thread::sleep(std::time::Duration::from_millis(2000));
        }
        // APK file not on device — proceed, screenshot may still work
    }

    // Remove stale forward first, then create fresh one
    let _ = adb()
        .args(["-s", serial, "forward", "--remove", U2_FORWARD])
        .output();
    let status = adb()
        .args(["-s", serial, "forward", U2_FORWARD, U2_FORWARD])
        .output()
        .map_err(|e| format!("adb not found: {e}"))?;
    if !status.status.success() {
        return Err(format!("adb forward ({U2_FORWARD}) failed — port may be in use"));
    }

    // Screenshot: GET /screenshot/{id} → PNG (via atx-agent HTTP, ~12% faster than adb exec-out)
    let screenshot_path = format!("/screenshot/{display_id}");
    let png_bytes = {
        let mut retries = 0;
        let mut last_err;
        loop {
            match http_get(&screenshot_path) {
                Ok(bytes) if bytes.starts_with(&[0x89, b'P', b'N', b'G']) => {
                    break bytes;
                }
                Ok(_) => last_err = "not a PNG".into(),
                Err(e) => last_err = e,
            }
            retries += 1;
            if retries >= 6 {
                return Err(format!("screenshot: {last_err}"));
            }
            std::thread::sleep(std::time::Duration::from_millis(800));
        }
    };
    std::fs::write(&png, &png_bytes).map_err(|e| format!("write png: {e}"))?;
    tmp_guard.track(png.clone());

    // Hierarchy dump: GET /dump/hierarchy → JSON-wrapped XML
    let raw = http_get("/dump/hierarchy")
        .map_err(|e| format!("dump: {e}"))?;
    let xml_bytes = extract_xml(&raw);
    std::fs::write(&xml, &xml_bytes).map_err(|e| format!("write xml: {e}"))?;
    tmp_guard.track(xml.clone());

    tmp_guard.disarm();
    Ok((png, xml))
}

impl App {
    fn load_screenshot(&mut self, path: &Path, ctx: &egui::Context) {
        if let Some(p) = self.temp_screenshot.take() {
            let _ = std::fs::remove_file(&p);
        }
        let dir = path.parent().map(|p| p.to_path_buf());
        match load_texture(path, ctx) {
            Ok((tex, _size)) => {
                self.screenshot_texture = Some(tex);
                self.screenshot_path = Some(path.to_path_buf());
            }
            Err(e) => {
                eprintln!("Failed to load screenshot: {e}");
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
        if let Some(p) = self.temp_xml.take() {
            let _ = std::fs::remove_file(&p);
        }
        let dir = path.parent().map(|p| p.to_path_buf());
        match std::fs::read_to_string(path) {
            Ok(content) => {
                // Detect multi-display format: <displays>, or <hierarchy> with <display> children
                self.file_displays = get_display_ids_from_xml(&content);
                if !self.file_displays.is_empty() {
                    self.file_xml_content = Some(content.clone());
                    self.tree_display_id = self.file_displays[0];
                    self.last_tree_display_id = self.tree_display_id;
                    self.root_node = parse_windows_xml(&content, self.tree_display_id);
                    if self.root_node.is_none() {
                        eprintln!("Failed to parse --windows XML for display {}", self.tree_display_id);
                    }
                } else {
                    self.file_displays.clear();
                    self.file_xml_content = None;
                    self.root_node = parse_xml(&content);
                    if self.root_node.is_none() {
                        eprintln!("Failed to parse XML — invalid format?");
                    }
                }
                self.xml_path = Some(path.to_path_buf());
            }
            Err(e) => {
                eprintln!("Failed to read XML: {e}");
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

            let ok_png = std::fs::copy(&screenshot, &out_png).is_ok();
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

    fn refresh_devices(&mut self) {
        let now = Instant::now();
        if self.last_adb_check.is_some_and(|t| now - t < std::time::Duration::from_secs(15)) {
            return;
        }
        self.last_adb_check = Some(now);
        self.adb_devices.clear();
        if let Ok(out) = adb()
            .args(["devices"])
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines().skip(1) {
                if let Some(serial) = line.split('\t').next() {
                    if !serial.is_empty() && line.contains("\tdevice") {
                        self.adb_devices.push(serial.to_string());
                    }
                }
            }
        }
        if !self.adb_devices.contains(&self.selected_device.as_deref().unwrap_or_default().to_string()) {
            self.selected_device = self.adb_devices.first().cloned();
        }
        if let Some(ref serial) = self.selected_device {
            let ids = get_displays(serial);
            if ids != self.available_displays {
                self.available_displays = ids;
                if !self.available_displays.iter().any(|(log, _)| *log == self.display_id) {
                    self.display_id = self.available_displays[0].0;
                }
            }
        }
    }

    fn load_from_u2(&mut self, ctx: &egui::Context) {
        let serial = match self.selected_device.clone() {
            Some(s) => s,
            None => {
                self.status_message = Some("No device selected — connect a device and ensure it appears in the dropdown".into());
                self.status_is_error = true;
                return;
            }
        };

        let old_png = self.temp_screenshot.take();
        let old_xml = self.temp_xml.take();

        match uiautomator2_capture(&serial, self.display_id) {
            Ok((png, xml)) => {
                self.load_screenshot(&png, ctx);
                self.load_xml(&xml, ctx);
                self.tree_display_id = self.display_id;
                self.last_tree_display_id = self.tree_display_id;
                self.temp_screenshot = Some(png);
                self.temp_xml = Some(xml);
                self.status_message = Some("uiautomator2 capture successful".into());
                self.status_is_error = false;
            }
            Err(e) => {
                self.temp_screenshot = old_png;
                self.temp_xml = old_xml;
                U2_STARTED.store(false, Ordering::Relaxed);
                self.status_message = Some(format!("u2 capture failed: {e}"));
                self.status_is_error = true;
            }
        }
    }

    fn adb_capture_and_load(&mut self, ctx: &egui::Context) {
        let serial = match self.selected_device.clone() {
            Some(s) => s,
            None => {
                self.status_message = Some("No device selected — connect a device and ensure it appears in the dropdown".into());
                self.status_is_error = true;
                return;
            }
        };

        if U2_STARTED.load(Ordering::Relaxed) {
            let _ = adb()
                .args(["-s", &serial, "shell", "kill -9 $(pidof atx-agent) 2>/dev/null; true"])
                .output();
            let _ = adb()
                .args(["-s", &serial, "shell", "am force-stop com.github.uiautomator"])
                .output();
            U2_STARTED.store(false, Ordering::Relaxed);
        }

        self.status_message = Some("Capturing...".into());
        self.status_is_error = false;

        let old_png = self.temp_screenshot.take();
        let old_xml = self.temp_xml.take();

        let phys_id = self.available_displays.iter()
            .find(|(log, _)| *log == self.display_id)
            .map(|(_, phys)| *phys)
            .unwrap_or(0);

        match adb_capture(&serial, self.display_id, phys_id) {
            Ok((png, xml)) => {
                self.load_screenshot(&png, ctx);
                self.load_xml(&xml, ctx);
                self.tree_display_id = self.display_id;
                self.last_tree_display_id = self.tree_display_id;
                self.temp_screenshot = Some(png);
                self.temp_xml = Some(xml);
                self.status_message = Some("ADB capture successful".into());
                self.status_is_error = false;
            }
            Err(e) => {
                self.temp_screenshot = old_png;
                self.temp_xml = old_xml;
                self.status_message = Some(format!("ADB capture failed: {e}"));
                self.status_is_error = true;
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.refresh_devices();

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
                match self.last_capture {
                    Some(CaptureMethod::Adb) => self.adb_capture_and_load(&ctx),
                    Some(CaptureMethod::U2) | None => self.load_from_u2(&ctx),
                }
            } else {
                ctx.request_repaint();
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
        if self.file_displays.len() > 1 && self.tree_display_id != self.last_tree_display_id {
            self.last_tree_display_id = self.tree_display_id;
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
                if ui.button("🔄 Refresh device").clicked() {
                    self.last_adb_check = None;
                    self.refresh_devices();
                }
                if !self.adb_devices.is_empty() {
                    let current = self.selected_device.as_deref().unwrap_or("");
                    egui::ComboBox::from_id_salt("device_selector")
                        .selected_text(current)
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
                }
                if !self.adb_devices.is_empty() {
                    let current_disp = format!("Disp {}", self.display_id);
                    egui::ComboBox::from_id_salt("display_selector")
                        .selected_text(&current_disp)
                        .width(70.0)
                        .show_ui(ui, |ui| {
                            for &(log, _phys) in &self.available_displays {
                                let label = format!("Disp {log}");
                                ui.selectable_value(&mut self.display_id, log, label);
                            }
                        });
                }
                ui.separator();
                if ui.button("📱 ADB Capture").clicked() {
                    self.last_capture = Some(CaptureMethod::Adb);
                    self.adb_capture_and_load(&ctx);
                }
                if ui.button("⚡ u2 Capture").clicked() {
                    self.last_capture = Some(CaptureMethod::U2);
                    self.load_from_u2(&ctx);
                }
                if ui.button("💾 Save").clicked() {
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
            let mut expanded = self.expanded.clone();
            egui::ScrollArea::vertical()
                .id_salt("tree_scroll")
                .max_height(tree_max)
                .auto_shrink([true, false])
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
                        for (path, rect) in &node_rects {
                            if rect.contains(pos) {
                                self.selected_path = Some(path.clone());
                                self.last_selected = Some(path.clone());
                                break;
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
                    let moved = self.last_hover_img_pos.map_or(true, |last| (img_pos - last).length() >= 5.0);
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
                        let same_spot = self.click_pos.map_or(false, |last| (img_pos - last).length() < 10.0);
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
            .with_decorations(true),
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

    // Cleanup: kill atx-agent, stop uiautomator service, remove forward
    if let Ok(guard) = U2_SERIAL.lock() {
        if let Some(ref serial) = *guard {
            let _ = adb()
                .args(["-s", serial, "shell", "kill -9 $(pidof atx-agent) 2>/dev/null; true"])
                .output();
            let _ = adb()
                .args(["-s", serial, "shell", "am force-stop com.github.uiautomator"])
                .output();
            let _ = adb()
                .args(["-s", serial, "forward", "--remove", U2_FORWARD])
                .output();
        }
    }

    result
}
