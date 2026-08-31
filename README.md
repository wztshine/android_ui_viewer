# UI Viewer

一款用 Rust 编写的 Android UI 布局检查桌面工具，基于
[egui](https://github.com/emilk/egui)（即时模式 GUI）。
单文件架构（`src/main.rs`，约 3200 行）。

## 功能

- **截图显示** — 加载设备截图，悬停/点击检查元素
- **元素高亮** — 悬停红色轮廓，选中绿色轮廓
- **可折叠树视图** — 浏览 UI 层级结构，支持展开/折叠，点击标签选中节点
- **属性检查** — 查看 class、text、resource-id、content-desc 及所有 XML 属性
- **ADB 捕获** — 一键截图 + uiautomator dump，支持多屏设备
- **uiautomator2 捕获** — 通过 u2.jar JSON-RPC 截图 + 层级 dump，服务端在多次捕获间保持常驻复用
- **双击操作** — 图片上双击发送 `input tap` 到设备并自动刷新
- **滑动操作** — 图片上拖拽发送 `input swipe` 到设备并自动刷新（阈值 10px 防误触）
- **持续监控** — 按固定间隔（0.5–60s）自动捕获，通过轻量层级探测做变更检测
- **XPath 显示** — 在属性面板显示悬停/选中元素的 XPath
- **导出图标** — 从截图裁剪选中元素并保存为 PNG
- **多屏支持** — 自动检测物理/逻辑显示 ID，下拉选择显示，ADB/U2/文件加载均支持
- **手动加载** — 从本地加载已有截图和 XML（自动配对同一目录的文件）
- **另存为** — 导出为 PNG + XML
- **跨平台** — 支持 Linux、Windows、macOS

## 构建与运行

```sh
# 调试构建（编译快）
cargo build

# 发布构建（优化）
cargo build --release
./target/release/uiviewer

# 直接构建并运行
cargo run
```

## 使用方法

1. **加载文件** — 点击 `Load Screenshot` 和 `Load XML` 选择匹配的截图和 dump 文件
2. **ADB 捕获** — 连接设备后从下拉列表选择设备，点击 `ADB` 自动截图 + dump
3. **U2 捕获** — 连接设备后点击 `U2`，通过 u2.jar 捕获（需安装 uiautomator2 相关工具）
4. **多屏** — 工具栏下拉选择显示 ID
5. **检查** — 在截图上悬停高亮元素，点击选中（同位置点击循环上溯父级）
6. **交互** — 双击发送 tap 指令，拖拽发送 swipe 指令
7. **浏览树** — 点击箭头展开/折叠，点击标签选中，自动滚动到选中节点
8. **属性** — 在右侧属性面板查看选中元素的属性和 XPath（分隔栏可拖拽调整宽度）
9. **导出图标** — 选中元素后，点击属性面板的 `✂️ Export Icon` 从截图裁剪该元素
10. **持续监控** — 勾选工具栏 `Keep monitor` 按固定间隔自动捕获，`-`/`+` 调整间隔
11. **保存** — 点击 `Save` 导出当前截图和 XML

也可以离线使用预捕获的文件浏览和检查，无需连接设备。

## 依赖

- [eframe/egui](https://github.com/emilk/egui) — GUI 框架
- [roxmltree](https://github.com/RazrFalcon/roxmltree) — XML 解析
- [image](https://github.com/image-rs/image) — 图片加载（PNG、JPEG）
- [rfd](https://github.com/PolyMeilex/rfd) — 原生文件对话框
- [serde_json](https://github.com/serde-rs/json) — JSON-RPC 响应解析（uiautomator2）
- [base64](https://github.com/marshallpierce/rust-base64) — u2 截图 base64 解码

## 系统要求

- Rust **1.92+**（MSRV，由 eframe 0.35 决定）
- 已安装 ADB 并加入 PATH（设备交互需要）
- OpenGL 3.2+（或 Vulkan/DirectX 12 via wgpu）
- 已连接的 Android 设备并开启 USB 调试（ADB / U2 捕获需要）
- **U2 捕获**：设备上需运行 u2.jar，安装方式：`python -m uiautomator2 init`

> **注意**：多屏功能已实现但**未经过实际多屏设备测试**。物理显示 ID 映射
>（`dumpsys SurfaceFlinger --display-id`）、`--windows` XML 解析以及显示选择器
> 均为代码完整但功能未经验证。
