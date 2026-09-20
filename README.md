# Rudder

**简体中文** | [English](./README.en.md)

> 本项目 Fork 自 [yituorou/meatshell](https://github.com/yituorou/meatshell)，
> 在原版基础上进行了以下优化和技术栈迁移：
>
> - **终端引擎迁移**：从 `vt100` 0.15 完全迁移至 `alacritty_terminal` 0.26，原生支持 Scrollback、Selection、Reflow
> - **自研 Selection 替换**：用 alacritty 原生 `Selection` + `selection_to_string()` 替代自定义选区管线
> - **原生 Reflow**：用 `Term::resize` 替代原始字节流重放，窗口缩放不再裁切输出
> - **渲染缓存**：行级 `RenderedLine` 缓存，静态画面跳过正则高亮重建

## 截图

<p align="center">
  <img src="docs/screenshots/01-welcome.png" alt="欢迎页 / 会话管理" width="800"><br>
  <em>欢迎页：会话管理 + 左侧本机资源监控</em>
</p>

<p align="center">
  <img src="docs/screenshots/02-terminal-htop.png" alt="终端 + SFTP" width="800"><br>
  <em>多标签页终端（htop 全屏渲染）+ 底部 SFTP 文件浏览 + 远端资源监控</em>
</p>

## 下载与安装

每次打 `v*` 标签，GitHub Actions 会自动构建 **Windows / Linux / macOS** 三平台二进制，
发布到 [Releases](https://github.com/SZhenY/Rudder/releases) 页面。

### Windows

下载 `rudder-*-windows-x86_64.zip`，解压后双击 `rudder.exe`。

### Linux

```bash
tar -xzf rudder-*-linux-x86_64.tar.gz
cd rudder-*-linux-x86_64
./rudder                                  # 直接运行
# 可选：装应用图标 + 启动器入口（Dock / 应用列表里显示图标，无需传参）
chmod +x install-linux.sh && ./install-linux.sh
```

> 需要 glibc ≥ 2.35（Ubuntu 22.04+ / Debian 12+）。Wayland 下首次装完图标可能要注销重登一次。

从源码 `cargo run`（Linux Mint / Ubuntu / Debian）需要先安装 Slint/winit/rfd 等用到的系统开发包：

```bash
sudo apt update
sudo apt install -y --no-install-recommends \
  build-essential pkg-config cmake \
  libfontconfig1-dev libfreetype6-dev \
  libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
  libgl1-mesa-dev libegl1-mesa-dev libgtk-3-dev \
  libudev-dev
```

### macOS

下载得到的是 `.zip`，里面是 `rudder.app` 应用程序包：

```bash
# 解压(aarch64 = Apple 芯片，x86_64 = Intel)
unzip rudder-*-macos-*.zip
# 移到「应用程序」(可选，留在原地也行)
mv rudder.app /Applications/
# 去掉「未签名应用」的隔离属性，否则会提示「rudder 已损坏，无法打开」
xattr -dr com.apple.quarantine /Applications/rudder.app
# 打开(或在「访达」里双击)
open /Applications/rudder.app
```

> 若未移到 `/Applications`，把上面两条路径换成 `.app` 实际所在位置(如 `~/Downloads/rudder.app`)即可。

> 从源码构建见下方 [运行](#运行)。

## 功能

### 已实现

- [x] FinalShell 风格 UI，深色 / 浅色 / 跟随系统主题
- [x] 本机 + 远端资源监控（CPU / 内存 / 交换 / 网络 / 磁盘）
- [x] 远端进程监控（按 CPU 排序、PID 复制与权限确认后结束进程）
- [x] 完整 VT/ANSI 终端模拟（btop / htop / vim 全屏正常渲染）
- [x] 彩色 emoji（支持肤色、旗帜及 ZWJ 组合序列）
- [x] 多标签页（欢迎页 + 多个会话）
- [x] 会话管理：新建 / 编辑 / 删除 / 分组，本地 JSON 持久化，导出 / 导入
  - 配置位置：`%APPDATA%/rudder/sessions.json`（Windows）
    / `~/.config/rudder/sessions.json`（Linux）
    / `~/Library/Application Support/rudder/sessions.json`（macOS）
- [x] SSH（`russh`，纯 Rust）：密码 / 私钥 / 加密私钥（密码短语）
- [x] SFTP 文件浏览 + 上传 / 下载（拖拽）+ 终端内 ZMODEM（`sz`）接收
- [x] SSH 端口转发 / 隧道：本地 -L / 远程 -R / 动态 -D（SOCKS5）
- [x] 快捷命令 + 命令输入框（可群发到所有会话）+ 命令历史
- [x] 串口 / Telnet 会话
- [x] 出站代理（SOCKS5 / HTTP）
- [x] 导入 `~/.ssh/config`
- [x] 会话密码加密存储（ChaCha20-Poly1305）
- [x] 已知主机（`known_hosts`）校验 + 首次连接确认
- [x] 多标签页终端分屏
- [x] **在线一键更新**：设置内手动检查更新，横幅「立即更新」→ 下载（实时进度）→ 自动替换 → 重启；失败自动回退浏览器下载
- [x] **连接失败原因分类**：认证失败 / 超时 / 端口被拒 / 网络不可达 / 域名解析失败 / 主机密钥不符，附排查建议（不再只显示原始错误码）
- [x] SFTP 下载断点续传（取消 / 失败保留半截文件，重试自动续传）
- [x] 终端双击选词 / 三击选行；查找支持 Enter / Shift+Enter 上下导航
- [x] 自定义输出高亮规则（关键词或正则、颜色、区分大小写、整行），规则可单独启停
- [x] 回滚缓冲上限可调：常规 10 万行，需要检索超长输出时可开启「大回滚缓冲区」（最高 100 万行）
- [x] 沉浸壁纸（内置简约·浅 / 暗，支持自定义图片）+ 主题色随壁纸派生
- [x] 平台字体栈：界面自动使用平台默认字体（macOS SF Pro Text → Helvetica Neue + Heiti SC、Windows Segoe UI + DengXian、Linux Ubuntu/Cantarell + Noto Sans CJK），终端默认 JetBrains Mono（含暗淡文本的 ExtraLight 变体）；额外字体可放入字体目录，见[自定义字体](#自定义字体外置字体)
- [x] Windows on ARM64（zip + MSI 安装包）
- [x] 平台视觉适配：macOS 更圆润的圆角 / 悬浮细滚动条 / 柔和卡片阴影，统一自绘风格
- [x] **渲染档位只有「GPU / 软件」两档**：GPU 走 wgpu（macOS Metal / Windows D3D12 / Linux
  Vulkan），Windows / Linux 首次启动自动探测，没有可用 GPU 时自动落到软件渲染

彩色 emoji 图形来自 [Twemoji](https://github.com/jdecked/twemoji)，按
[CC BY 4.0](https://creativecommons.org/licenses/by/4.0/) 使用；完整署名见
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。

### 计划中

- [ ] 会话密码改用 OS 钥匙串存储

## 自定义字体（外置字体）

CJK 大字体（Maple Mono 这类每个字重约 20 MB）不再打进安装包。需要额外字体时，
把字体文件放进「字体目录」，**重启 Rudder** 后即可在设置里选用。

### 字体目录

| 平台 | 路径 |
| --- | --- |
| Windows（安装版 / 便携版） | `<rudder.exe 所在目录>\config\fonts` — 启动时自动创建，不会写入 AppData |
| macOS | `~/Library/Application Support/dev.rudder.rudder/fonts` — 启动时自动创建 |
| Linux | `~/.config/rudder/fonts` — 设了 `XDG_CONFIG_HOME` 时在其下 |

macOS 另有一个**只读的备用位置**：`Rudder.app/Contents/MacOS/rudder/config/fonts`
（就在可执行文件路径之下）。`/Applications` 通常不可写，所以这个目录不会自动创建，
只在你手工建好之后才会被扫描 —— 适合把字体跟 App 一起打包分发。

### 支持格式

`.ttf` / `.otf`（单字体）以及 `.ttc` / `.otc`（字体集合），扩展名不区分大小写。
集合文件里的**每个字重都会被注册**，所以一个文件可能一次出现多个家族名。

### 怎么选

放好文件并重启后：

- **终端字体**：设置 → 终端 → 终端字体（内嵌 + 外置 + 系统**等宽**字体）
- **界面字体**：设置 → 外观 → 界面字体（内嵌 + 外置 + 系统**全部**字体）

下拉框按来源分三段：`▍内嵌字体`（JetBrains Mono / Meatshell Mono）、
`▍外置字体`（你放进去的）、`▍系统字体`（系统已安装的）。

几个要知道的点：

- 列表里显示的是**字体文件内部的家族名**，不是文件名。例如
  `MapleMono-NF-CN-Regular.ttf` 会显示成 `Maple Mono NF CN`。
- 同名家族只出现一次，优先级 **内嵌 > 外置 > 系统** —— 放入同名文件可以覆盖内嵌字体那一项。
- 终端字体列表里**外置字体不做等宽过滤**（只有系统字体过滤）。放进比例字体也能选中，
  但终端按网格排版，显示会很难看。
- **外置字体不会被自动选用**：界面字体的默认值是平台字体栈，需要在设置里手动选择。
- **暗淡文本（SGR-2）取的是 `字体名 + " Thin"`** 这个家族，不是字重。想让暗淡文本有真正的
  细体效果，要一并放入该字体的 Thin 字重；像 Maple Mono 这种 Thin 单独成家族的
  （`Maple Mono Normal NL NF CN Thin`），它会作为**另一个条目**出现在列表里，属正常。

### 终端里的中文

终端字体自身不含中文时，中文与全角标点会**自动回退到界面字体**，不会出现方块。
判断依据是家族名里是否带 `CN` / `SC` / `TC` / `JP` / `KR` / `CJK` / `Han` 标记：

- **名字带标记**（如 `Maple Mono NF CN`）→ 中文直接用该字体渲染，斜体 / 细体等变体
  对中文同样生效；
- **名字不带标记** → 中文回退到界面字体（内嵌的 JetBrains Mono 就属于这一类）。

想让中文也用终端字体渲染，请选择名字带上述标记的字体。

### 放了字体但列表里看不到？

1. **重启了吗** —— 字体目录只在启动时扫描一次。
2. **扩展名对不对** —— 只认 `.ttf` / `.otf` / `.ttc` / `.otc`（`.woff2` 等不支持）。
3. **确认家族名，而不是文件名** —— 用系统字体查看器看文件内部的名字，再在列表里找。
4. **macOS 的 App 旁目录是只读备用位置** —— 只有手工创建后才会被扫描。

## 技术栈

| 模块          | 选型                                                              |
| ------------- | ----------------------------------------------------------------- |
| UI            | [Slint](https://slint.dev) 1.18（纯 Rust 编译，无 GC）           |
| 渲染          | **GPU**：FemtoVG on [wgpu](https://wgpu.rs)（macOS Metal / Windows D3D12 / Linux Vulkan）；**软件**：tiny-skia + softbuffer |
| 终端模拟      | [`alacritty_terminal`](https://crates.io/crates/alacritty_terminal) 0.26（VT/ANSI + 原生 scrollback/reflow） |
| PTY           | `portable-pty` 0.9（跨平台伪终端）                                |
| 异步运行时    | [`tokio`](https://tokio.rs) 1.x（rt-multi-thread）                |
| SSH 协议      | [`russh`](https://crates.io/crates/russh) 0.49（纯 Rust，无 libssh；0.62.6 升级分支 `russh-0.62` 待合入） |
| SFTP          | `russh-sftp` 2.4                                                   |
| 系统指标      | [`sysinfo`](https://crates.io/crates/sysinfo) 0.38                 |
| 序列化        | `serde` + `serde_json`                                             |
| 日志          | `tracing` + `tracing-subscriber`                                   |
| 密码加密      | `chacha20poly1305`（会话密码）+ `aes`/`argon2`/`hmac`/`sha2`（PuTTY PPK） |
| 表情符号      | `twemoji-assets` 1.5（内嵌 PNG）                                   |
| 代理          | `tokio-socks` 5（SOCKS5）                                          |
| 串口          | `serialport` 4                                                     |
| 系统字体      | `fontdb` 0.23                                                      |
| 图像解码      | `image` 0.25（PNG/JPEG/WebP/BMP 壁纸）                             |

### 渲染

设置 → 界面 → 渲染 只有两档，三平台一致：

- **GPU**（macOS 默认）：FemtoVG 走 wgpu，底下的图形接口由 wgpu 选平台原生那个 —— macOS
  **Metal**、Windows **D3D12**、Linux **Vulkan**。
- **软件**：CPU 渲染（tiny-skia + softbuffer）。GPU 档在某台机器上有问题时的备用档。

Windows / Linux 的默认是**首次启动探测一次**：枚举适配器看机器上有没有真 GPU（WARP / lavapipe
这类软件适配器不算），再真渲染一帧验证；结论写进配置文件，之后按配置启动，判定不通过就落到软件渲染。

**逃生口**：万一某台机器上 GPU 档异常（黑屏 / 花屏 / 窗口打不开），用

```bash
SLINT_BACKEND=winit-software ./rudder
```

启动，或在设置里把渲染切成「软件」（环境变量优先级高于配置里的 `renderer_mode`）。

## 运行

```bash
cargo run --release
```

首次启动会在配置目录建立 `settings.json`（设置）与 `sessions.json`（会话）。
点击右上
角 **“＋ 新建会话”** 添加第一台服务器。

## 项目布局

```
rudder/
├── Cargo.toml
├── build.rs                     # Slint 编译 + 依赖版本生成
├── ui/                          # Slint 界面定义（重型块都在各自文件里）
│   ├── app.slint                # 顶层窗口：装配标题栏 / 主体 / 覆盖层
│   ├── terminal_view.slint      # 终端视图 + SFTP dock
│   ├── terminal_parts.slint     # 终端相关的可复用部件
│   ├── sftp_panel.slint         # SFTP 文件浏览面板
│   ├── sftp_parts.slint         #   面板内的行 / 列表部件
│   ├── sidebar.slint            # 左侧系统监控面板（外壳 + 折叠）
│   ├── sidebar_body.slint       #   侧栏正文（资源 / 进程）
│   ├── sidebar_blocks.slint     #   侧栏行块（网速 / 磁盘 / 进程行等）
│   ├── tabs.slint               # 顶部标签栏
│   ├── welcome.slint            # 欢迎页 / 快速连接
│   ├── welcome_rows.slint       #   会话行 / 分组行
│   ├── session_dialog.slint     # 新建 / 编辑会话弹框
│   ├── session_types.slint      #   会话相关的共享 struct
│   ├── dialog_fields.slint      #   弹框字段（各类输入行）
│   ├── custom_titlebar.slint    # 自绘标题栏
│   ├── titlebar_inset_strip.slint  # 标题栏内嵌条（无边框窗口）
│   ├── interface_panel.slint    # 设置面板外壳（左侧导航 + 内容区）
│   ├── interface_settings_overlay.slint  # 侧栏直接打开的设置覆盖层
│   ├── settings/                # 设置面板实现（按页拆分）
│   │   ├── chrome.slint         #   通用控件：行 / 段标题 / 步进器 / 色板
│   │   ├── section.slint        #   分区容器（设置项分组的最小单位）
│   │   ├── reset_bar.slint      #   「还原本页默认」两段式确认按钮
│   │   ├── types.slint          #   跨组件共享的 struct
│   │   └── pages/               #   8 个设置页，一页一文件
│   ├── widgets.slint            # 可复用组件
│   ├── theme.slint              # 设计 tokens（深色/浅色）
│   ├── proc_window.slint        # 进程管理窗口
│   ├── system_info_window.slint # 系统信息窗口
│   ├── confirm_dialog.slint     # 通用确认 / 删除对话框
│   ├── confirm_overlays.slint   #   删除 / 批量导入 / 粘贴 / 凭据 / MFA / 主机密钥确认
│   ├── quick_cmd_manage.slint   # 快捷命令管理弹窗（含分组菜单）
│   ├── group_dialog.slint       # 新建 / 重命名分组
│   ├── quick_group_dialog.slint # 快捷命令分组命名
│   ├── rename_dialog.slint      # 重命名会话
│   ├── sftp_prompt_dialog.slint # SFTP 重命名 / 新建目录 / 新建文件
│   ├── chmod_dialog.slint       # SFTP 改权限
│   ├── shortcuts_dialog.slint   # 快捷键一览
│   ├── download_manager.slint   # 下载管理器
│   ├── settings_menu.slint      # 右上角设置菜单
│   ├── about_dialog.slint       # 关于
│   ├── update_banner.slint      # 更新提示横幅
│   ├── file_editor.slint        # 内置文件查看 / 编辑器
│   ├── confirm_close_dialog.slint  # 关闭活动会话前的确认
│   ├── dock_snap_overlay.slint  # 拖拽停靠时的吸附指示层
│   └── fonts/                   # 内嵌字体
├── lang/                        # 国际化
│   ├── zh/                      # 简体中文
│   └── en/                      # English
└── src/
    ├── main.rs                  # 入口
    ├── app.rs                   # UI ↔ 后端桥接（核心控制器）
    ├── app/                     # 桥接层拆出的回调模块（会话、设置页、自更新…）
    ├── terminal/                # 终端模拟子系统
    │   └── impls/
    │       ├── vt_adapter.rs    # alacritty_terminal 封装
    │       ├── term_buffer.rs   # 终端缓冲区（scrollback/渲染/缓存）
    │       ├── render.rs        # 行构建（build_row/build_line）
    │       ├── presentation.rs  # 终端输出渲染 + emoji
    │       ├── input.rs         # 键盘输入编码（PTY/IME/Ctrl）
    │       ├── local.rs         # 本地 shell（portable-pty）
    │       ├── serial.rs        # 串口终端
    │       ├── telnet.rs        # Telnet
    │       ├── zmodem.rs        # ZMODEM 文件传输
    │       ├── render_gate.rs   # 帧同步栅栏
    │       └── output_highlight.rs  # 日志/DevOps 级别着色
    ├── ssh/                     # SSH 子系统
    │   └── impls/
    │       ├── ssh.rs           # SSH 会话 worker
    │       ├── known_hosts.rs   # 主机密钥校验
    │       ├── ppk.rs           # PuTTY PPK 私钥加载
    │       ├── proxy.rs         # 出站代理（SOCKS5）
    │       └── ssh_config.rs    # ~/.ssh/config 导入
    ├── sftp/                    # SFTP 文件浏览 + 上传/下载
    ├── session/                 # 会话管理（JSON 持久化、加密存储）
    ├── tunnel/                  # SSH 端口转发（-L/-R/-D）
    ├── resource/                # 系统资源监控（CPU/内存/交换/磁盘/网络）
    ├── config/                  # 配置管理
    ├── i18n/                    # 国际化
    ├── layout/                  # 窗口布局 / 分屏
    ├── logging/                 # tracing 初始化
    ├── ui/                      # Slint UI 辅助类型（TermSpan/Match）
    ├── wallpaper/               # 自定义背景图
    └── webdav/                  # WebDAV 客户端
```

## 开发提示

- Slint 控件有非常严格的布局 DSL，改 `.slint` 后 `cargo check` 是最快的
  反馈方式。
- 应用事件循环是单线程（Slint 要求），所有跨线程 UI 更新通过
  `slint::invoke_from_event_loop` 回调。
- SSH / SFTP 共享 `known_hosts` 校验逻辑：首次连接会确认并记住主机密钥，
  后续密钥变化会再次提示。

## 发版

不要直接手动修改 `Cargo.toml` 后再打标签。使用发布脚本，让 Git tag 指向的提交本身就已经包含正确版本号：

```powershell
.\scripts\release.ps1 v0.7.9 -Push
```

脚本会更新 `Cargo.toml` / `Cargo.lock`，运行 `cargo check --locked`，验证 `rudder --version`，提交 `Release v0.7.9`，创建 annotated tag。
没有 PowerShell 的环境照这五步手工做即可（顺序一致）：改两个版本号 → `cargo check --locked` →
`cargo run --locked -- --version` 应输出 `rudder <版本>` → `git commit -m "Release v<版本>"` +
`git tag -a v<版本> -m "Release v<版本>"` → 推分支与 tag。，并推送当前分支和 tag。更多细节见 [docs/release.md](docs/release.md)。

## 开发方式

Rudder 由维护者与 AI 编程助手（[WorkBuddy](https://www.workbuddy.cn)，GLM 模型）协作开发：

- **维护者主导**：产品方向、架构决策、代码审查、实机验证与发布
- **AI 完成**：功能实现、缺陷修复、测试编写、CI/CD 维护与文档——AI 根据维护者的需求描述生成全部变更
- 所有改动均经过本地构建验证、GitHub Actions 6 平台构建矩阵与人工确认后才发布

## License

MIT OR Apache-2.0（双许可）。
