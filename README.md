# UI Viewer / Android UI 布局检查工具

A Rust desktop application for inspecting Android UI layouts from uiautomator XML dumps.
Built with [egui](https://github.com/emilk/egui) (immediate-mode GUI).
Single file (~1753 lines) located at `src/main.rs`.

一款 Rust 桌面工具，从 uiautomator XML dump 中检查 Android UI 布局。
基于 [egui](https://github.com/emilk/egui)（即时模式 GUI）构建。
单一文件（约 1753 行），位于 `src/main.rs`。

## Features / 功能

- **Screenshot display** — load a device screenshot and hover/click to inspect elements  
  **截图显示** — 加载设备截图，悬停/点击检查元素
- **Element highlighting** — red outline on hover, green outline on selection  
  **元素高亮** — 悬停红色边框，选中绿色边框
- **Collapsible tree view** — browse the UI hierarchy with expand/collapse arrows  
  **可折叠树状视图** — 展开/折叠浏览 UI 层级结构
- **Property inspection** — view class, text, resource-id, content-desc, and all XML attributes  
  **属性检查** — 查看 class、text、resource-id、content-desc 及所有 XML 属性
- **ADB capture** — one-click screenshot + uiautomator dump from a connected device  
  **ADB 捕获** — 一键截屏 + uiautomator dump
- **U2 capture** — capture via atx-agent JSON-RPC (screenshot + hierarchy dump)  
  **U2 捕获** — 通过 atx-agent JSON-RPC 捕获（截图 + 层级 dump）
- **Double-click tap** — double-click on image to send `input tap` and auto-refresh  
  **双击触控** — 双击图片发送 `input tap` 并自动刷新
- **Drag swipe** — drag on image to send `input swipe` and auto-refresh (10px threshold)  
  **拖拽滑动** — 拖拽图片发送 `input swipe` 并自动刷新（10px 阈值）
- **Multi-display** — automatic detection of physical/logical display IDs  
  **多显示设备** — 自动检测物理/逻辑显示 ID
- **Manual load** — load existing screenshot + XML pair from filesystem (auto-pairs)  
  **手动加载** — 从文件系统加载截屏 + XML 对（自动配对同目录文件）
- **Save-as** — export current screenshot + XML pair via file dialog  
  **另存为** — 通过文件对话框导出当前截图 + XML 对
- **Cross-platform** — works on Linux, Windows, and macOS  
  **跨平台** — 支持 Linux、Windows 和 macOS

## Build & Run / 构建与运行

```sh
# Debug build (fast compilation) / 调试构建（快速编译）
cargo build

# Release build (optimized) / 发布构建（优化）
cargo build --release
./target/release/uiviewer

# Build and run directly / 直接构建并运行
cargo run
```

## Usage / 使用说明

1. **Load files / 加载文件** — click `Load Screenshot` and `Load XML` to select files (auto-pairs in same directory)  
   点击 `Load Screenshot` 和 `Load XML` 选择文件（自动配对同目录文件）
2. **ADB Capture** — connect a device via USB, select it from the dropdown, click `ADB`  
   通过 USB 连接设备，从下拉菜单选择，点击 `ADB`
3. **U2 Capture** — connect a device with atx-agent installed, click `U2`  
   连接已安装 atx-agent 的设备，点击 `U2`
4. **Multi-display / 多显示设备** — toolbar combo box sets capture display; properties panel combo box filters tree  
   工具栏选择器设置捕获显示；属性面板选择器过滤树状视图
5. **Inspect / 检查** — hover to highlight; click to select (same spot cycles ancestors)  
   悬停高亮，单击选中（同位置循环遍历祖先）
6. **Interact / 交互** — double-click to tap; drag to swipe; auto-refresh after 800ms  
   双击触控，拖拽滑动，800ms 后自动刷新
7. **Browse tree / 浏览树** — click arrows to expand/collapse; click labels to select; auto-scrolls to selected  
   点击箭头展开/折叠，点击标签选中，自动滚动到选中节点
8. **Properties / 属性** — view selected element's attributes in the right panel  
   在右侧面板查看选中元素的属性
9. **Save / 保存** — click `Save` to export the current screenshot + XML pair  
   点击 `Save` 导出当前截图 + XML 对

The app also works completely offline with pre-captured files — no device needed for browsing and inspection.  
该应用也支持完全离线使用预捕获的文件——浏览和检查无需连接设备。

## Dependencies / 依赖

- [eframe/egui](https://github.com/emilk/egui) — GUI framework / GUI 框架
- [roxmltree](https://github.com/RazrFalcon/roxmltree) — XML parsing / XML 解析
- [image](https://github.com/image-rs/image) — image loading (PNG, JPEG) / 图片加载
- [rfd](https://github.com/PolyMeilex/rfd) — native file dialogs / 原生文件对话框
- [serde_json](https://github.com/serde-rs/json) — JSON-RPC response parsing / JSON-RPC 响应解析

## Requirements / 要求

- Rust **1.92+** (MSRV)
- ADB installed and in PATH / ADB 已安装并在 PATH 中
- OpenGL 3.2+ (or Vulkan/DirectX 12 via wgpu) for rendering / OpenGL 3.2+（或通过 wgpu 使用 Vulkan/DirectX 12）用于渲染
- Android device with USB debugging enabled / 开启 USB 调试的 Android 设备
- **For U2**: [atx-agent](https://github.com/openatx/atx-agent) on device (`python -m uiautomator2 init`)

> **Note / 注意**: Multi-display support is code-complete but has not been tested on actual multi-display devices.
> 多显示设备支持已实现但未在实际多显示设备上测试。

---

This repository's code was entirely written by **DeepSeek V4 Flash** in about 8 hours.
If the UI tree parsing has issues or other bugs occur, provide the UI tree file and
bug description to an AI to fix it.

此仓库代码由 **DeepSeek V4 Flash** 在约 8 小时内完成。如果 UI 树解析有问题或其他 bug，请提供 UI 树文件和 bug 描述给 AI 修复。

---

## Windows Notes / Windows 注意事项

**Console window suppression / 控制台窗口抑制:**  
On Windows, every ADB subprocess (e.g. `adb devices`, `screencap`, `pull`) previously flashed a console window. All ADB invocations now use a `CREATE_NO_WINDOW` flag via the `adb()` helper to suppress this.  
在 Windows 上，每次执行 ADB 子进程（如 `adb devices`、`screencap`、`pull`）原本会闪烁一个控制台窗口。现在所有 ADB 调用都通过 `adb()` 辅助函数，带有 `CREATE_NO_WINDOW` 标志来抑制此现象。

**Device refresh / 设备刷新:**  
The toolbar's 🔄 button forces a refresh of the device list and display IDs. The list also auto-refreshes every 15 seconds while the app is running. Refreshes run on a background thread, so the UI never blocks while querying adb.  
工具栏的 🔄 按钮可强制刷新设备列表和显示 ID。列表在运行期间也会每 15 秒自动刷新一次。刷新在后台线程执行，查询 adb 时 UI 不会卡顿。

**Background capture / 后台捕获:**  
Captures (ADB/U2) run on a background thread, keeping the UI responsive. A capture that hangs is abandoned after 30 seconds (`CAPTURE_TIMEOUT`) and the previous view is restored.  
捕获（ADB/U2）在后台线程执行，UI 保持响应。挂起的捕获会在 30 秒（`CAPTURE_TIMEOUT`）后被放弃并恢复之前的视图。

**Rust version / Rust 版本:**  
Requires Rust **1.92+** (MSRV dictated by eframe 0.35).  
需要 Rust **1.92 及以上**（MSRV 由 eframe 0.35 决定）。
