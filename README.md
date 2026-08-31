# UI Viewer / Android UI 布局检查工具

A Rust desktop application for inspecting Android UI layouts from uiautomator XML dumps.
Built with [egui](https://github.com/emilk/egui) (immediate-mode GUI).

一款 Rust 桌面工具，从 uiautomator XML dump 中检查 Android UI 布局。
基于 [egui](https://github.com/emilk/egui)（即时模式 GUI）构建。

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
- **U2 capture** — capture via uiautomator2 (u2.jar JSON-RPC); warm server reuse across captures  
  **U2 捕获** — 通过 uiautomator2（u2.jar JSON-RPC）捕获；服务端在多次捕获间保持常驻复用
- **Double-click tap** — double-click on image to send `input tap` and auto-refresh  
  **双击触控** — 双击图片发送 `input tap` 并自动刷新
- **Drag swipe** — drag on image to send `input swipe` and auto-refresh (10px threshold)  
  **拖拽滑动** — 拖拽图片发送 `input swipe` 并自动刷新（10px 阈值）
- **Keep monitor** — auto-capture at a fixed interval (0.5–60s), change-detection via a cheap hierarchy probe  
  **持续监控** — 按固定间隔（0.5–60s）自动捕获，通过轻量层级探测做变更检测
- **XPath display** — shows the XPath of the hovered/selected element in the Properties panel  
  **XPath 显示** — 在属性面板显示悬停/选中元素的 XPath
- **Export Icon** — crop the selected element from the screenshot and save as PNG  
  **导出图标** — 从截图中裁剪选中元素并保存为 PNG
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
3. **U2 Capture** — connect a device with u2.jar installed, click `U2`  
   连接已安装 u2.jar 的设备，点击 `U2`
4. **Multi-display / 多显示设备** — toolbar combo box sets capture display; properties panel combo box filters tree  
   工具栏选择器设置捕获显示；属性面板选择器过滤树状视图
5. **Inspect / 检查** — hover to highlight; click to select (same spot cycles ancestors)  
   悬停高亮，单击选中（同位置循环遍历祖先）
6. **Interact / 交互** — double-click to tap; drag to swipe; auto-refresh after 1s (settle delay)  
   双击触控，拖拽滑动，1 秒（settle delay）后自动刷新
7. **Browse tree / 浏览树** — click arrows to expand/collapse; click labels to select; auto-scrolls to selected  
   点击箭头展开/折叠，点击标签选中，自动滚动到选中节点
8. **Properties / 属性** — view selected element's attributes and XPath in the right panel  
   在右侧面板查看选中元素的属性和 XPath
9. **Export Icon / 导出图标** — with an element selected, click `✂️ Export Icon` in the Properties panel to crop it from the screenshot  
   选中元素后，点击属性面板中的 `✂️ Export Icon` 从截图裁剪该元素
10. **Keep monitor / 持续监控** — tick `Keep monitor` in the toolbar to auto-capture at a fixed interval; `-`/`+` adjust the interval  
    勾选工具栏中的 `Keep monitor` 按固定间隔自动捕获；`-`/`+` 调整间隔
11. **Save / 保存** — click `Save` to export the current screenshot + XML pair  
    点击 `Save` 导出当前截图 + XML 对

The app also works completely offline with pre-captured files — no device needed for browsing and inspection.  
该应用也支持完全离线使用预捕获的文件——浏览和检查无需连接设备。

## Dependencies / 依赖

- [eframe/egui](https://github.com/emilk/egui) — GUI framework / GUI 框架
- [roxmltree](https://github.com/RazrFalcon/roxmltree) — XML parsing / XML 解析
- [image](https://github.com/image-rs/image) — image loading (PNG, JPEG) / 图片加载
- [rfd](https://github.com/PolyMeilex/rfd) — native file dialogs / 原生文件对话框
- [serde_json](https://github.com/serde-rs/json) — JSON-RPC response parsing / JSON-RPC 响应解析
- [base64](https://github.com/marshallpierce/rust-base64) — u2 screenshot base64 decoding / u2 截图 base64 解码

## Requirements / 要求

- Rust **1.92+** (MSRV)
- ADB installed and in PATH / ADB 已安装并在 PATH 中
- OpenGL 3.2+ (or Vulkan/DirectX 12 via wgpu) for rendering / OpenGL 3.2+（或通过 wgpu 使用 Vulkan/DirectX 12）用于渲染
- Android device with USB debugging enabled / 开启 USB 调试的 Android 设备
- **For U2**: u2.jar on device, installed via `python -m uiautomator2 init`

> **Note / 注意**: Multi-display support is code-complete but has not been tested on actual multi-display devices.
> 多显示设备支持已实现但未在实际多显示设备上测试。
