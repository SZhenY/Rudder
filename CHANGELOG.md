# Changelog / 更新日志

All notable changes are documented here. 本文件记录所有重要变更。
中英对照（English first, 中文在后）.

## [0.2.2] - 2026-06-05

### Security / 安全

- **Fix Windows command injection (#12)** — `open_with_os` no longer shells out
  via `cmd /C start`; it calls `ShellExecuteW` directly so a malicious remote
  file name (e.g. `foo&calc.exe`) can't inject commands. Added `sanitize_filename`
  as defence-in-depth.
  **修复 Windows 命令注入 (#12)** —— 打开文件不再经 `cmd /C start`，改用
  `ShellExecuteW` 直接打开，恶意远程文件名（如 `foo&calc.exe`）无法注入命令；
  并新增 `sanitize_filename` 清洗作为纵深防御。

- **Stop echoing the saved password when editing a session (#10)** — the field
  is left blank with a "leave blank to keep" hint; an empty field on save keeps
  the existing password.
  **编辑会话时不再回显已保存密码 (#10)** —— 密码框留空并提示「留空则不修改」，
  保存时为空则保留原密码。

- **Zero passwords in memory on drop (#8)** — passwords now use a `Secret` type
  (`zeroize`) that wipes its heap buffer on drop and redacts itself in logs; the
  on-disk JSON format is unchanged.
  **密码内存清零 (#8)** —— 密码改用 `Secret` 类型（`zeroize`），Drop 时清零堆
  内存、日志中脱敏；磁盘 JSON 格式不变。

### Added / 新增

- **Internationalization — Chinese / English with runtime switching (#9).**
  Static UI uses Slint `@tr` + bundled `.po`; dynamic Rust strings use a `t()`
  helper. Switch via the gear menu; the choice is persisted and the default
  follows the system locale.
  **国际化 —— 中 / 英双语，运行时实时切换 (#9)。** 静态界面用 Slint `@tr` +
  bundled `.po`；Rust 动态文本用 `t()`。设置菜单里切换，选择会持久化，首次启动
  跟随系统语言。

- **Private-key file picker** in the session dialog, plus `.pub` fallback (auto
  strips the suffix to load the matching private key) and uniform `/` path
  separators across platforms.
  **会话弹窗的私钥文件选择器**，并支持 `.pub` 容错（自动去后缀加载对应私钥）、
  路径分隔符统一为 `/`。

- **Linux desktop integration** — `assets/meatshell.desktop` + `install-linux.sh`
  and an `xdg_app_id` so the GNOME/Ubuntu dock shows the app icon on Wayland.
  **Linux 桌面集成** —— `assets/meatshell.desktop` + `install-linux.sh`，并设置
  `xdg_app_id`，使 Wayland 下 GNOME/Ubuntu 任务栏显示应用图标。

- **Screenshots in the README** (`docs/screenshots/`, sensitive info redacted).
  **README 增加截图**（`docs/screenshots/`，敏感信息已打码）。

[0.2.2]: https://github.com/jeff141/meatshell/releases/tag/v0.2.2
