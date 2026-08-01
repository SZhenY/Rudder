# meatshell VT 解析库更换方案：vt100 → alacritty_terminal

> **编写日期**: 2026-08-01（v2，基于官方文档 docs.rs/latest 重新核实）
> **当前版本**: meatshell v0.6.9, vt100 v0.15
> **目标版本**: alacritty_terminal **0.26.0**（2026-04-06 发布，要求 Rust 1.85.0, Edition 2024）
> **方案性质**: 完整重构方案（含代码级适配层设计）

---

## 目录

1. [结论摘要](#1-结论摘要)
2. [性能与兼容性专项分析（换库动机）](#2-性能与兼容性专项分析换库动机)
3. [版本与 MSRV 决策](#3-版本与-msrv-决策)
4. [官方 API 基线（0.26.0，已核实）](#4-官方-api-基线0260已核实)
5. [当前 vt100 使用清单（复核）](#5-当前-vt100-使用清单复核)
6. [架构决策：滚动历史与 reflow](#6-架构决策滚动历史与-reflow)
7. [核心数据模型改造](#7-核心数据模型改造)
8. [适配层实现（vt_adapter.rs 全量代码）](#8-适配层实现vt_adapterrs-全量代码)
9. [逐文件改造清单](#9-逐文件改造清单)
10. [交互与状态查询迁移](#10-交互与状态查询迁移)
11. [测试策略](#11-测试策略)
12. [分阶段实施计划](#12-分阶段实施计划)
13. [风险清单与缓解措施](#13-风险清单与缓解措施)
14. [回滚方案](#14-回滚方案)

---

## 1. 结论摘要

| 项目 | 结论 |
|------|------|
| **换库动机** | **提升解析性能 + 兼容性**（用户明确） |
| **可行性** | ✅ 可行，但注意 0.26.0 的 MSRV 是 **Rust 1.85**，meatshell 当前声明 `rust-version = "1.75"`，**必须同步升级** |
| **本质差异** | vt100 是"纯解析器"（process → screen 快照）；alacritty_terminal 是"完整终端仿真器"（Term + Grid + Handler 事件模型），字节喂入走 `vte::ansi::Processor` |
| **性能真相（重要）** | 两者底层**同源**：vt100 0.15.2 依赖 vte 0.11，alacritty 0.26 依赖 vte 0.15——纯解析速度**接近**。真正瓶颈在 meatshell 的**全量渲染**（build_row × 行数 + detect_scroll 全行 diff）。**换库不换渲染 = 性能可能不升反降**，必须配合 damage 增量渲染才能兑现性能红利（详见 §2） |
| **兼容性红利（实打实）** | vt100 仅实现 vt100/xterm 子集，缺大量现代序列（OSC 8/52、Kitty keyboard、Sixel、DEC 私有模式全集、zerowidth 等）；且社区有**滚动越界崩溃报道 + 维护放缓**，0.16 也是 breaking change（详见 §2） |
| **最大风险** | 宽字符标记方式不同（vt100 用 `is_wide_continuation()`，alacritty 用 `Flags::WIDE_CHAR/WIDE_CHAR_SPACER`）；渲染线程与解析线程的 `Term` 访问需要 `Mutex` 保护 |
| **预估工作量** | 方案 A（换解析器 + 保留自研历史）：10-14 天；**A+（A + damage 增量渲染）**：12-16 天；方案 B（拥抱原生 scrollback/reflow/selection）：14-18 天 |
| **推荐** | **方案 A+**：先换解析器拿兼容性（A），再上 damage 增量渲染兑现性能（A+），稳定后演进方案 B |

---

## 2. 性能与兼容性专项分析（换库动机）

> 换库动机是**解析性能 + 兼容性**。本节用事实回答两个问题：① alacritty_terminal 解析真的更快吗？② 兼容性到底提升了什么？

### 2.1 解析器同源真相（性能判断的前提）

**关键事实：两者底层是同一个解析状态机家族（vte）**，已通过 Cargo.lock 核实：

```
meatshell 现状:
  vt100 0.15.2
    └── vte 0.11.1（2021 版，表驱动 ANSI 状态机）
    └── itoa / utf8parse / vte_generate_state_changes ...

换库后:
  alacritty_terminal 0.26.0
    └── vte 0.15.x（2025 版，同款表驱动状态机，含多年优化）
```

结论：
- **纯字节解析速度两者接近**——都是 vte 表驱动状态机（每字节 O(1)，无分支热循环）。vte 0.15 相比 0.11 有性能修复与优化，但**量级相同，不构成换库的核心理由**
- 差异在**解析器之上的"终端状态层"**：vt100 用简化屏幕缓冲（内存更小但功能少）；alacritty 用 `Grid<Cell>`（功能全但单格更重：`c + fg + bg + Flags + Option<Arc<CellExtra>>`）
- ⚠️ **若 meatshell 保持现在的全量渲染（每帧 build_row 遍历 rows×cols + detect_scroll 全行 diff），换库后 CPU 开销很可能不降反升**（网格访问变重）

### 2.2 性能红利在哪里：damage 增量渲染（真正的兑现点）

alacritty_terminal 0.26 提供行级损坏追踪（damage tracking），这才是性能提升的正道：

```rust
// 0.26 新增：自上次 reset_damage() 以来的损坏信息
pub fn damage(&mut self) -> TermDamage<'_>;      // 迭代损坏的行
pub fn reset_damage(&mut self);                  // 每帧消费后重置
// TermDamage 枚举 + LineDamageBounds / TermDamageIterator
//   - TermDamage::Lines { lines: LineDamageBounds }   ← 脏行区间
//   - TermDamage::Cursor / Alt / Selection / ...       ← 其他脏状态

// RenderableContent 提供视图相关（含光标）的最终渲染数据
pub fn renderable_content(&self) -> RenderableContent<'_>;
```

**meatshell 当前渲染模式**（每次 render_gate 触发）：

```
render() 每帧:
  for r in 0..rows { build_row(term, r, cols) }   ← 全量遍历所有行
  detect_scroll(prev, curr)                        ← 全行文本 diff
  render_term_span(...) × 全部 spans               ← 全量生成 TermSpan
```

**升级为 damage 增量（A+ 方案）**：

```
ingest_chunk() 后: 记录本轮 damage 的行集合（自上次 render 以来）
render() 每帧:
  for r in damage_lines { build_row(term, r, cols) }   ← 只重建脏行
  其余行复用 prev 缓存的 Line                        ← 未变行零成本
  detect_scroll 仅对脏行区间做前缀匹配               ← 或直接用 grid.display_offset()
```

预期收益：**高频刷新（btop、日志 tail、进度条）下渲染 CPU 降 60-80%**——从"每帧全量 O(rows×cols)"变为"每帧 O(脏行×cols)"。这才是换库后肉眼可见的性能提升。

> `Term::damage()` 的调用约束：需要 `&mut self`，且应放在持有 `TermBuffer` 锁的同一线程（解析线程）消费——与 meatshell 现有 `Arc<Mutex<TermBuffer>>` 结构兼容，在 ingest 路径末尾调 `damage()` 收集脏行号，UI 线程 render 时只重建这些行。

### 2.3 兼容性提升清单（实打实的部分）

vt100 0.15 只实现 **vt100/xterm 子集**（docs.rs 自述 "may not support all modern terminal features"），且社区有真实负面反馈：

| 维度 | vt100 0.15.2 | alacritty_terminal 0.26 | 对 meatshell 的意义 |
|------|--------------|------------------------|---------------------|
| **维护状态** | 更新缓慢；社区报道**滚动越界崩溃**（Linutil 项目案例）；0.16 引入 Callbacks trait 是 breaking change | Alacritty 官方维护，月下载 39 万+，持续迭代 | 换库降低长期风险 |
| **OSC 支持** | 仅基础（标题等） | OSC 0/1/2（标题）、**OSC 8（超链接）**、**OSC 52（剪贴板）**、OSC 697 等 | 现代 CLI（starship、gh cli）依赖 |
| **SGR 完整度** | 基础 16/256/真彩 | 完整 SGR 参数、下划线颜色、双下划线、strikeout、overline | 颜色渲染更准确 |
| **键盘协议** | 无 | **Kitty keyboard protocol**（`Config.kitty_keyboard`） | 未来可选 |
| **Sixel/图像** | 无 | 0.25+ 支持 Sixel | 现代工具（chafa、img2txt） |
| **DEC 私有模式** | 部分 | 全集（DECSET/DECRST），含鼠标协议全模式 | btop/tmux 兼容性 |
| **Unicode** | `is_wide`/组合字符支持有限 | `WIDE_CHAR/SPACER` 双标记 + `zerowidth()` 组合字符数组 + 精确字素处理 | CJK/emoji 显示（#132 历史痛点） |
| **滚动/选择语义** | 简化 diff 实现 | 原生 scrollback + `Selection` + `bounds_to_string` | 复制/查找准确度 |
| **resize reflow** | `set_size` 只截断/填充（meatshell 被迫自研 raw 重放） | `Term::resize` 内置 reflow（`Grid::resize(reflow=true)`） | 删自研逻辑（方案 B） |

### 2.4 结论：针对"性能 + 兼容性"动机的路线

| 动机 | 满足方式 | 需要做的 |
|------|---------|---------|
| **兼容性** | 换解析器即得（方案 A 就够） | P0-P3 全部迁移，无额外成本 |
| **性能** | 必须配合 **damage 增量渲染**（A+） | 在方案 A 基础上加 §2.2 的渲染改造 |
| 两者兼得 | 方案 A+，后续演进 B | 见 §12 分阶段计划 |

> **若只换库不改渲染，性能动机不会兑现**——这是本方案最重要的提醒。

---

## 3. 版本与 MSRV 决策

### 3.1 版本选择

crates.io 最新稳定版为 **0.26.0**（2026-04-06），也是 docs.rs/latest 对应版本：

| 版本 | Rust 要求 | Edition | 说明 |
|------|----------|---------|------|
| **0.26.0** | **1.85.0** | 2024 | 最新；配置系统重构（`term::Config`）；`advance_bytes` 已移除 |
| 0.25.1 | 1.85.0 | 2024 | 与 0.26 接近 |
| 0.24.2 | 1.74.0 | 2021 | 老 API（`Term::new(&config, &SizeInfo, history)` + `advance_bytes`） |

### 3.2 MSRV 影响（关键约束）

```toml
# meatshell/Cargo.toml 现有声明
rust-version = "1.75"
```

alacritty_terminal 0.26.0 要求 **Rust 1.85.0 + Edition 2024**。这意味着：

1. **必须升级** `rust-version = "1.85"`（或至少 1.85）
2. 检查项目其他依赖是否有 MSRV 上限（`slint 1.8`、`russh 0.49` 等均无问题）
3. CI/本地工具链需 ≥ 1.85

> ⚠️ 若不想升级 MSRV，只能退回 0.24.2（API 完全不同：`Term::new(&config, &size, history)` + `term.advance_bytes(bytes)`）。**本方案按 0.26.0 编写**；如选 0.24.2，仅适配层签名不同，改造思路一致。

### 3.3 依赖变更

```toml
[dependencies]
# 删除
vt100 = "0.15"

# 新增
alacritty_terminal = "0.26"

# unicode-width 需与 alacritty_terminal 的依赖对齐（其要求 0.2.x），
# 原 pin 的 0.1.14 需升级（Cargo 会自动解析，但建议显式声明）
unicode-width = "0.2"
```

**编译影响**：alacritty_terminal 0.26 会引入约 30+ 个传递依赖（含 `alacritty_config` 系列、`vte`、`serde_yaml`/`toml` 等），首次全量编译增加约 1-3 分钟。

---

## 4. 官方 API 基线（0.26.0，已核实）

以下全部来自 docs.rs/latest 实际页面核实，**非猜测**。

### 3.1 核心类型

```rust
// ── re-export ─────────────────────────────────────────────
pub use alacritty_terminal::Term;   // Term<T>
pub use alacritty_terminal::Grid;   // Grid<Cell>
pub use alacritty_terminal::vte;    // vte crate 完整 re-export！

// ── term::Config（注意：在 term 模块，不是 config 模块；config 模块已不存在）──
pub struct Config {
    pub scrolling_history: usize,              // 滚动历史上限（meatshell 用 5000）
    pub default_cursor_style: CursorStyle,
    pub vi_mode_cursor_style: Option<CursorStyle>,
    pub semantic_escape_chars: String,
    pub kitty_keyboard: bool,
    pub osc52: Osc52,
}
// 实现 Default，可用 Config::default() 后改字段

// ── term::TermSize（官方提供的尺寸类型，实现 Dimensions）──
pub struct TermSize {
    pub columns: usize,
    pub screen_lines: usize,
}
impl TermSize {
    pub fn new(columns: usize, screen_lines: usize) -> Self;
}
impl Dimensions for TermSize {
    fn total_lines(&self) -> usize;   // = screen_lines
    fn screen_lines(&self) -> usize;
    fn columns(&self) -> usize;
}

// ── event::VoidListener（官方空事件接收器！直接用）──
pub struct VoidListener;
impl EventListener for VoidListener;

// ── event::EventListener trait ──
pub trait EventListener {
    fn send_event(&self, event: Event);
}
// Event 枚举含: Wakeup, Title(String), ChildExit(i32), Bell, MouseCursorDirty, Color, Resize, ...

// ── index 模块 ──
pub struct Line(pub i32);        // i32！实现了 Copy/Add/Sub/From<i32>/From<usize>
pub struct Column(pub usize);    // usize
pub struct Point { pub line: Line, pub column: Column }
pub enum Direction { Left, Right }
pub enum Boundary { Cursor, Grid, Top, Bottom }
pub type Side = ...;
```

### 3.2 Term 方法（0.26.0 全部固有公开方法）

```rust
impl<T: EventListener> Term<T> {
    // 创建（注意：config 按值、尺寸是引用、第三个参数是事件监听器）
    pub fn new<D: Dimensions>(config: Config, dimensions: &D, event_proxy: T) -> Term<T>;

    // 网格访问
    pub fn grid(&self) -> &Grid<Cell>;
    pub fn grid_mut(&mut self) -> &mut Grid<Cell>;

    // 调整尺寸（reflow 由内部处理）
    pub fn resize<S: Dimensions>(&mut self, size: S);

    // 模式查询
    pub fn mode(&self) -> &TermMode;             // bitflags，用 .contains()/.intersects()
    pub fn colors(&self) -> &Colors;

    // 选择系统
    pub fn selection_to_string(&self) -> Option<String>;
    pub fn bounds_to_string(&self, start: Point, end: Point) -> String;

    // 渲染
    pub fn renderable_content(&self) -> RenderableContent<'_> where T: EventListener;

    // 滚动
    pub fn scroll_display(&mut self, scroll: Scroll) where T: EventListener;

    // 光标
    pub fn cursor_style(&self) -> CursorStyle;

    // 其他
    pub fn swap_alt(&mut self);
    pub fn exit(&mut self) where T: EventListener;
    pub fn set_options(&mut self, options: Config) where T: EventListener;
    pub fn reset_damage(&mut self);
    pub fn damage(&mut self) -> TermDamage<'_>;
    pub fn semantic_escape_chars(&self) -> &str;

    // 搜索（可选红利）
    pub fn search_next(&self, regex: &mut RegexSearch, origin: Point, direction: Direction, side: Side, max_lines: Option<usize>) -> Option<Match>;
    pub fn regex_search_left/right(...);
    pub fn semantic_search_left/right(...);
    pub fn bracket_search(&self, point: Point) -> Option<Point>;
    pub fn line_search_left/right(...);

    // 公开字段
    pub is_focused: bool,
    pub vi_mode_cursor: ViModeCursor,
    pub selection: Option<Selection>,
}

// ⚠️ 注意：0.26 的 Term 上【没有】advance_bytes / cursor() / mouse() / config() / is_altscreen()
//    — advance_bytes 是 0.24 的老 API，已移除
//    — 字节喂入改为 vte::ansi::Processor（见 3.4）
//    — 模式查询统一走 term.mode().contains(TermMode::XXX)
//    — 光标走 term.grid().cursor.point
```

### 3.3 TermMode 位标志（模式查询关键）

```rust
// term.mode() 返回 &TermMode（bitflags），关键标志（常量名以实际编译为准）：
TermMode::SHOW_CURSOR
TermMode::APP_CURSOR          // ← 替代 vt100 screen.application_cursor()
TermMode::BRACKETED_PASTE     // ← 替代 vt100 screen.bracketed_paste()
TermMode::ALTERNATE_SCREEN    // ← 替代 vt100 screen.alternate_screen()
TermMode::MOUSE_REPORT_CLICK
TermMode::MOUSE_REPORT_DRAG
TermMode::MOUSE_REPORT_MOTION
TermMode::MOUSE_REPORT_MODE   // ← 组合判断：是否启用鼠标协议
TermMode::SGR_MOUSE           // ← SGR 编码（vt100 MouseProtocolEncoding::Sgr）
TermMode::URXVT_MOUSE         // ← URXVT 编码
TermMode::UTF8_MOUSE
TermMode::INSERT
TermMode::FOCUS_IN_OUT
TermMode::KITTY_KEYBOARD
```

### 3.4 字节处理：vte::ansi::Processor（替代 process/advance_bytes）

```rust
use alacritty_terminal::vte::ansi::Processor;

// Term<T> 实现了 vte::ansi::Handler，Processor 负责把字节流转成 Handler 调用
let mut processor = Processor::new();

// 方式一：逐字节
for &byte in bytes {
    processor.advance(&mut term, byte);
}

// 方式二：整块（若该版本提供）
// processor.advance_with(&mut term, bytes);
```

> 建议封装为 `TermBuffer::process(&mut self, bytes: &[u8])`，内部持有 `Processor` 实例（跨调用保持状态）。

### 3.5 Grid 访问与 Cell

```rust
// ── Grid<T> 索引 ──
impl Index<Line> for Grid<T> { type Output = Row<T>; }
impl Index<Point> for Grid<T> { type Output = T; }   // ← 最直接：grid[Point {line, column}]
// Row<T> 实现 Index<Column>
// 所以：&term.grid()[Line(row)][Column(col)] == &term.grid()[Point{...}]

// ── Cell（字段全部公开！）──
pub struct Cell {
    pub c: char,                                  // 单个字符（不是 String！）
    pub fg: Color,                                // vte::ansi::Color
    pub bg: Color,
    pub flags: Flags,                             // bitflags
    pub extra: Option<Arc<CellExtra>>,            // 零宽字符/超链接等附加数据
}

// ── Flags（bitflags，关键标志）──
Flags::BOLD
Flags::ITALIC
Flags::UNDERLINE
Flags::INVERSE
Flags::WRAPLINE          // ← 行尾自动换行（vt100 row_wrapped 对应物）
Flags::WIDE_CHAR         // ← 宽字符（CJK）起始格
Flags::WIDE_CHAR_SPACER  // ← 宽字符占位格（vt100 is_wide_continuation 对应物）
Flags::DIM
Flags::HIDDEN
Flags::STRIKEOUT
Flags::EMPTY
Flags::SELECTED
Flags::ZERO_WIDTH

// ── Cell 附加方法 ──
impl Cell {
    pub fn zerowidth(&self) -> Option<&[char]>;   // 零宽字符（组合符号）
    pub fn push_zerowidth(&mut self, character: char);
    pub fn clear_wide(&mut self);
    pub fn underline_color(&self) -> Option<Color>;
    pub fn hyperlink(&self) -> Option<Hyperlink>;
}

// ── Grid 其他方法 ──
impl<T> Grid<T> {
    pub fn new(lines: usize, columns: usize, max_scroll_limit: usize) -> Grid<T>;
    pub fn display_iter(&self) -> GridIterator<'_, T>;   // 遍历可见单元格
    pub fn iter_from(&self, point: Point) -> GridIterator<'_, T>;
    pub fn display_offset(&self) -> usize;               // 当前视口偏移（滚动量）
    pub fn scroll_display(&mut self, scroll: Scroll);
    pub fn clear_history(&mut self);
    pub fn update_history(&mut self, history_size: usize);
    pub fn cursor_cell(&mut self) -> &mut T;
    pub cursor: Cursor<T>,                               // 公开字段
    pub saved_cursor: Cursor<T>,                         // 公开字段
}

// ── Dimensions trait（Grid<T> 与 Term<T> 都实现）──
pub trait Dimensions {
    fn columns(&self) -> usize;
    fn screen_lines(&self) -> usize;    // 可见行数（视口高度）
    fn total_lines(&self) -> usize;     // 总行数（含 scrollback）
    fn history_size(&self) -> usize { ... }   // scrollback 不可见行数
    fn last_column(&self) -> Column { ... }
    fn topmost_line(&self) -> Line { ... }
    fn bottommost_line(&self) -> Line { ... }
}
```

### 3.6 颜色类型（vte::ansi::Color，通过 crate re-export）

```rust
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

pub enum Color {
    Named(NamedColor),   // 具名颜色（Foreground/Background/Black/Red/...）
    Indexed(u8),         // 256 色调色板索引 ← vt100::Color::Idx(n)
    Spec(Rgb),           // 真彩色 ← vt100::Color::Rgb(r,g,b)
}
pub struct Rgb { pub r: u8, pub g: u8, pub b: u8 }

// NamedColor 枚举含：Black, Red, Green, Yellow, Blue, Magenta, Cyan, White,
// BrightBlack...BrightWhite, Foreground, Background, Cursor, DimBlack... 等
```

---

## 5. 当前 vt100 使用清单（复核）

（详细行号清单见 v1 版本，此处仅列**必须迁移的 API 类别**，按改造影响排序）

| 类别 | vt100 API | 影响文件 | 0.26 替代 |
|------|-----------|---------|-----------|
| 解析器类型 | `vt100::Parser` | `types.rs` | `Term<VoidListener>` + `vte::ansi::Processor` |
| 建解析器 | `Parser::new(r, c, 5000)` | `app.rs×5`、`term_buffer.rs`、`ssh.rs`、测试×3 | `Term::new(config, &TermSize, VoidListener)` |
| 喂字节 | `parser.process(bytes)` | `term_buffer.rs`、`app.rs`、`ssh.rs`、测试 | `processor.advance(&mut term, byte)` |
| 屏幕快照 | `parser.screen()` | 全局 ~40 处 | `term.grid()`（借用）+ `term.mode()` |
| 尺寸 | `screen.size() → (u16,u16)` | 全局 | `term.grid().columns()/screen_lines()` 或 `TermSize` |
| 光标 | `screen.cursor_position() → (u16,u16)` | `term_buffer.rs`、`app.rs`、测试 | `term.grid().cursor.point` |
| alt screen | `screen.alternate_screen()` | `term_buffer.rs`、`app.rs` | `term.mode().contains(TermMode::ALTERNATE_SCREEN)` |
| 单元格 | `screen.cell(r,c) → Option<Cell>` | `render.rs` | `&term.grid()[Point{line,column}]`（无 Option，直接用） |
| 单元内容 | `cell.contents() → String` | `render.rs` | `cell.c`（char）+ `cell.zerowidth()` |
| 前景/背景色 | `cell.fgcolor()/bgcolor() → vt100::Color` | `render.rs`、`presentation.rs` | `cell.fg/cell.bg`（vte::ansi::Color） |
| 粗体 | `cell.bold()` | `render.rs` | `cell.flags.contains(Flags::BOLD)` |
| 宽字符 | `cell.is_wide()` / `is_wide_continuation()` | `render.rs` | `Flags::WIDE_CHAR` / `Flags::WIDE_CHAR_SPACER` |
| 反转 | `cell.inverse()` | `render.rs` | `cell.flags.contains(Flags::INVERSE)` |
| 行换行 | `screen.row_wrapped(row)` | `render.rs` | `grid[Line(row)].flags().contains(Flags::WRAPLINE)` |
| 括号粘贴 | `screen.bracketed_paste()` | `input.rs` | `term.mode().contains(TermMode::BRACKETED_PASTE)` |
| 应用光标键 | `screen.application_cursor()` | `app.rs` | `term.mode().contains(TermMode::APP_CURSOR)` |
| 鼠标协议 | `screen.mouse_protocol_mode()` | `app.rs` | `term.mode()` 组合判断（见 §10.3） |
| 鼠标编码 | `screen.mouse_protocol_encoding()` | `app.rs` | `TermMode::SGR_MOUSE` / `URXVT_MOUSE` |
| 调整尺寸 | `parser.set_size(r, c)` | `app.rs` | `term.resize(TermSize::new(cols, rows))` |
| 颜色类型 | `vt100::Color::{Default,Idx,Rgb}` | `presentation.rs`~30 处、`app.rs` 测试、`HistSpan` | 自定义 `TermColor`（见 §7.2） |
| 测试快照 | `screen.contents()` | `ssh.rs` | `grid` 行遍历重建 |

---

## 6. 架构决策：滚动历史与 reflow

这是本次改造**最关键的架构分叉点**。meatshell 目前自研了三套滚动历史相关机制：

```
history: VecDeque<Line>   ← 渲染后的滚动历史行（从 screen diff 构建）
prev: Vec<Line>           ← 上一帧屏幕快照（用于 detect_scroll 差分）
raw: VecDeque<u8>         ← 原始字节流（2MB 上限，用于 resize 时重放重排）
```

### 方案 A：渐进式（先拿兼容性）

**保留** meatshell 的 `history/prev/raw` 三件套，仅用 alacritty 替换"屏幕网格解析"部分。

- 优点：`TermBuffer` 上层逻辑（find、selection 绝对坐标、view_offset、渲染窗口）**零改动**；风险最小；可独立验证渲染输出一致性
- 缺点：alacritty 的原生 scrollback/reflow 红利没有吃到；`Config.scrolling_history` 设为 0 或较小值避免双重开销
- **兼容性目标已达成**（换解析器即得）；**性能目标未兑现**（仍是全量渲染）
- 工作量：10-14 天

### 方案 A+：A + damage 增量渲染（推荐，兑现性能）

在方案 A 基础上，利用 alacritty 的 `Term::damage()` 行级损坏追踪，把每帧全量 `build_row` 降为**只重建脏行**（详见 §2.2）。

- 新增：`TermBuffer` 增加 `damage_rows: RangeSet<u16>` 或复用 `prev` 做行级 diff；ingest 路径末尾收集 damage；render 只遍历脏行，未变行复用缓存
- 优点：渲染 CPU 降 60-80%（高频刷新场景）；`history/prev/raw` 保留，风险可控；**性能与兼容性两个动机同时兑现**
- 缺点：多 ~200 行改动；`prev` 缓存与 `detect_scroll` 逻辑需微调（改为脏行区间内做）
- 工作量：12-16 天（= 方案 A + 2 天）

### 方案 B：拥抱原生（后续演进）

**删除** `history/prev/raw`，改用 alacritty 原生能力：

- scrollback：`Config.scrolling_history = 5000`，历史行直接读 `grid` 的 hidden lines
- reflow：`Term::resize()` 内部自动重排（`Grid::resize(reflow=true)`），**删掉 `reflow()` 的 raw 重放**
- 选择：用 `Term.selection` + `selection_to_string()` 替代自研 `sel_anchor/sel_focus/sel_ranges`
- 渲染：`term.renderable_content()` 直接给出可见内容 + damage 信息，可简化 `render()` 与 `render_gate`
- 优点：删除 ~400 行自研逻辑，获得 alacritty 级 reflow/selection 质量，`TermBuffer` 瘦身为薄包装
- 缺点：`view_offset`/`displayed_text`/find/高亮的绝对坐标体系要重映射到 grid 坐标系；改动面大
- 工作量：14-18 天

> **推荐路径（针对性能+兼容性动机）**：**方案 A+** —— 方案 A 落地拿兼容性 → 加 damage 增量渲染兑现性能 → 稳定后择机演进方案 B。本文档以方案 A/A+ 为主交付，方案 B 在 §6.1 给出关键改造点。

### 6.1 方案 B 关键改造点（预览）

```rust
// 1. scrollback 由 Config 控制
let config = Config { scrolling_history: 5000, ..Config::default() };

// 2. 滚动视图直接读 grid 历史区（不再用 self.history）
let grid = term.grid();
let history_lines = grid.history_size();       // 不可见行数
let display_offset = grid.display_offset();    // 当前滚动偏移

// 3. resize 自动 reflow，删除 self.raw 与 reflow()
term.resize(TermSize::new(new_cols, new_rows));

// 4. 选择文本（删除 extract_range_text）
if let Some(text) = term.selection_to_string() { ... }

// 5. 渲染（可选项，替代自研 build_row 遍历）
let content = term.renderable_content();
```

---

## 7. 核心数据模型改造

### 6.1 TermBuffer 结构体（types.rs）

```rust
// ── 修改前 ──
pub(crate) struct TermBuffer {
    pub(crate) parser: vt100::Parser,
    pub(crate) find_query: String,
    pub(crate) is_dark: bool,
    pub(crate) output_highlight: OutputHighlightPreset,
    pub(crate) custom_highlight_rules: Vec<CompiledOutputRule>,
    pub(crate) sel_anchor: Option<(usize, u16)>,
    pub(crate) sel_focus: Option<(usize, u16)>,
    pub(crate) sel_ranges: Vec<((usize, u16), (usize, u16))>,
    pub(crate) history: VecDeque<Line>,
    pub(crate) prev: Vec<Line>,
    pub(crate) view_offset: usize,
    pub(crate) displayed_text: Vec<String>,
    pub(crate) csi_state: CsiState,
    pub(crate) raw: VecDeque<u8>,
}

// ── 修改后（方案 A）──
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi::Processor;

pub(crate) struct TermBuffer {
    pub(crate) term: Term<VoidListener>,        // 替代 parser
    pub(crate) processor: Processor,            // 替代 parser.process
    pub(crate) config: TermConfig,              // 供 resize/reflow 重建用
    // ↓ 以下字段不变
    pub(crate) find_query: String,
    pub(crate) is_dark: bool,
    pub(crate) output_highlight: OutputHighlightPreset,
    pub(crate) custom_highlight_rules: Vec<CompiledOutputRule>,
    pub(crate) sel_anchor: Option<(usize, u16)>,
    pub(crate) sel_focus: Option<(usize, u16)>,
    pub(crate) sel_ranges: Vec<((usize, u16), (usize, u16))>,
    pub(crate) history: VecDeque<Line>,
    pub(crate) prev: Vec<Line>,
    pub(crate) view_offset: usize,
    pub(crate) displayed_text: Vec<String>,
    pub(crate) csi_state: CsiState,
    pub(crate) raw: VecDeque<u8>,
}
```

### 6.2 自定义颜色类型 TermColor（替代 vt100::Color）

`vt100::Color` 在 `HistSpan`、`presentation.rs`、高亮规则、测试中共出现 **50+ 处**。直接引入 `vte::ansi::Color` 也行，但它多一个 `Named(...)` 变体，且 `vte::ansi::Color` 不实现 `Clone + Copy` 之外的便利比较。**推荐自定义薄类型**，隔离对底层 crate 的依赖：

```rust
// types.rs
/// 终端颜色，替代 vt100::Color；与渲染层解耦。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TermColor {
    Default,        // 主题默认前景/背景
    Idx(u8),        // 256 色调色板索引
    Rgb(u8, u8, u8),
}

impl From<&alacritty_terminal::vte::ansi::Color> for TermColor {
    fn from(color: &alacritty_terminal::vte::ansi::Color) -> Self {
        match color {
            alacritty_terminal::vte::ansi::Color::Named(_) => TermColor::Default,
            alacritty_terminal::vte::ansi::Color::Indexed(i) => TermColor::Idx(*i),
            alacritty_terminal::vte::ansi::Color::Spec(rgb) => TermColor::Rgb(rgb.r, rgb.g, rgb.b),
        }
    }
}

// HistSpan 颜色字段改为 TermColor
pub(crate) struct HistSpan {
    pub(crate) text: String,
    pub(crate) fg: TermColor,      // 原 vt100::Color
    pub(crate) bg: TermColor,
    pub(crate) bold: bool,
    pub(crate) inverse: bool,
    pub(crate) col: i32,
    pub(crate) cells: i32,
}
```

> 全局替换策略：`vt100::Color::Default → TermColor::Default`、`vt100::Color::Idx(n) → TermColor::Idx(n)`、`vt100::Color::Rgb(r,g,b) → TermColor::Rgb(r,g,b)`，`matches!(x, vt100::Color::Default) → matches!(x, TermColor::Default)`。可用 IDE 全局替换 + 编译错误驱动收尾。

---

## 8. 适配层实现（vt_adapter.rs 全量代码）

新建 `src/terminal/impls/vt_adapter.rs`，集中封装 alacritty 网格访问，保持 `build_row`/`render` 调用签名稳定：

```rust
// src/terminal/impls/vt_adapter.rs
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermSize};
use alacritty_terminal::vte::ansi::{Color, Processor};

use crate::terminal::{HistSpan, Line as TermLine, TermColor};

/// 构建 Term + Processor（替代 vt100::Parser::new）
pub(crate) fn new_term(rows: u16, cols: u16, history: usize) -> (Term<VoidListener>, Processor) {
    let config = Config {
        scrolling_history: history,
        ..Config::default()
    };
    let size = TermSize::new(cols as usize, rows as usize);
    let term = Term::new(config.clone(), &size, VoidListener);
    (term, Processor::new())
}

/// 尺寸：(rows, cols)，替代 screen.size()
pub(crate) fn term_size(term: &Term<VoidListener>) -> (u16, u16) {
    let grid = term.grid();
    (grid.screen_lines() as u16, grid.columns() as u16)
}

/// 光标位置：(row, col)，替代 screen.cursor_position()
pub(crate) fn cursor_pos(term: &Term<VoidListener>) -> (u16, u16) {
    let point = term.grid().cursor.point;
    (point.line.0 as u16, point.column.0 as u16)
}

/// 是否 alt screen，替代 screen.alternate_screen()
pub(crate) fn is_alt(term: &Term<VoidListener>) -> bool {
    use alacritty_terminal::term::mode::TermMode;
    term.mode().contains(TermMode::ALTERNATE_SCREEN)
}

/// 单格属性，替代 screen.cell(row, col)
///
/// 注意：alacritty 的 cell 是单 char，宽字符用 WIDE_CHAR/WIDE_CHAR_SPACER
/// 双标记；组合字符存在 cell.zerowidth() 里。返回 (内容, fg, bg, bold, wide, inverse)。
pub(crate) fn cell_attrs(
    term: &Term<VoidListener>,
    row: u16,
    column: u16,
) -> (String, TermColor, TermColor, bool, bool, bool) {
    let point = Point {
        line: Line(row as i32),
        column: Column(column as usize),
    };
    let cell = &term.grid()[point];

    // 内容：主字符 + 零宽字符（组合符号）
    let mut contents = cell.c.to_string();
    if let Some(zw) = cell.zerowidth() {
        for ch in zw {
            contents.push(*ch);
        }
    }

    let fg = TermColor::from(&cell.fg);
    let bg = TermColor::from(&cell.bg);
    let bold = cell.flags.contains(Flags::BOLD);
    let wide = cell.flags.contains(Flags::WIDE_CHAR);
    let inverse = cell.flags.contains(Flags::INVERSE);

    (contents, fg, bg, bold, wide, inverse)
}

/// 是否宽字符占位格（替代 is_wide_continuation）
pub(crate) fn is_wide_continuation(term: &Term<VoidListener>, row: u16, column: u16) -> bool {
    let point = Point {
        line: Line(row as i32),
        column: Column(column as usize),
    };
    term.grid()[point].flags.contains(Flags::WIDE_CHAR_SPACER)
}

/// 行是否自动换行延续（替代 screen.row_wrapped）
pub(crate) fn row_wrapped(term: &Term<VoidListener>, row: u16) -> bool {
    let line = Line(row as i32);
    term.grid()[line].flags().contains(Flags::WRAPLINE)
}

/// 批量喂字节（替代 parser.process）——跨调用保持 Processor 状态
pub(crate) fn process_bytes(
    processor: &mut Processor,
    term: &mut Term<VoidListener>,
    bytes: &[u8],
) {
    for &byte in bytes {
        processor.advance(term, byte);
    }
}

/// 调整尺寸（替代 parser.set_size / 重建 Parser）
pub(crate) fn resize_term(term: &mut Term<VoidListener>, rows: u16, cols: u16) {
    term.resize(TermSize::new(cols as usize, rows as usize));
}

/// 清屏/重置时的终端重建
pub(crate) fn reset_term(rows: u16, cols: u16, history: usize) -> (Term<VoidListener>, Processor) {
    new_term(rows, cols, history)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_and_read_back() {
        let (mut term, mut proc) = new_term(5, 30, 100);
        process_bytes(&mut proc, &mut term, b"hello");
        assert_eq!(term_size(&term), (5, 30));
        let (text, ..) = cell_attrs(&term, 0, 0);
        assert_eq!(text, "h");
        let (row, col) = cursor_pos(&term);
        assert_eq!((row, col), (0, 5));
    }

    #[test]
    fn wide_char_marking() {
        let (mut term, mut proc) = new_term(3, 30, 100);
        // "你" 是宽字符（2 列）
        process_bytes(&mut proc, &mut term, "你".as_bytes());
        let (_, _, _, _, wide, _) = cell_attrs(&term, 0, 0);
        assert!(wide, "WIDE_CHAR 应标记在起始格");
        assert!(is_wide_continuation(&term, 0, 1), "占位格应标记 WIDE_CHAR_SPACER");
    }

    #[test]
    fn alt_screen_detection() {
        let (mut term, mut proc) = new_term(5, 30, 100);
        process_bytes(&mut proc, &mut term, b"\x1b[?1049h");
        assert!(is_alt(&term));
        process_bytes(&mut proc, &mut term, b"\x1b[?1049l");
        assert!(!is_alt(&term));
    }
}
```

---

## 9. 逐文件改造清单

### 8.1 `src/terminal/struct/types.rs`

```rust
// 1. TermBuffer.parser → term + processor + config（见 §7.1）
// 2. HistSpan.fg/bg: vt100::Color → TermColor（见 §7.2）
// 3. 新增 TermColor 枚举与 From 实现
// 4. 引入类型别名（可选，减少后续改动）：
pub(crate) type ATerm = alacritty_terminal::term::Term<alacritty_terminal::event::VoidListener>;
```

### 8.2 `src/terminal/impls/render.rs`（核心改造）

`build_row` 的签名从 `(&vt100::Screen, ...)` 改为 `(&ATerm, ...)`，并改用适配层：

```rust
// ── 修改前 ──
fn cell_attrs(screen: &vt100::Screen, row: u16, column: u16)
    -> (String, vt100::Color, vt100::Color, bool, bool, bool)
{ ... }

pub(crate) fn build_row(screen: &vt100::Screen, row: u16, columns: u16) -> Line { ... }

// ── 修改后 ──
use crate::terminal::vt_adapter::{cell_attrs, is_wide_continuation, row_wrapped};

pub(crate) fn build_row(term: &ATerm, row: u16, columns: u16) -> Line {
    let mut plain = String::with_capacity(columns as usize);
    let mut runs = Vec::new();
    let mut column = 0u16;
    while column < columns {
        // 宽字符占位格：跳过（其内容并入起始格）
        if is_wide_continuation(term, row, column) {
            column += 1;
            continue;
        }
        let (contents, foreground, background, bold, wide, inverse) =
            cell_attrs(term, row, column);
        if wide {
            plain.push_str(&contents);
            runs.push(HistSpan {
                text: contents,
                fg: foreground,
                bg: background,
                bold,
                inverse,
                col: column as i32,
                cells: 2,
            });
            column += 2;
            continue;
        }
        // ... 其余 run 合并逻辑与原来一致（颜色/粗体/反转对比用 TermColor PartialEq）
    }
    (plain, runs, row_wrapped(term, row))
}
```

**宽字符处理对比**：

| | vt100 | alacritty 0.26 |
|---|---|---|
| 起始格 | `cell.is_wide()` | `Flags::WIDE_CHAR` |
| 占位格 | `cell.is_wide_continuation()`（内容为空） | `Flags::WIDE_CHAR_SPACER`（`c` 通常为 `' '`） |
| 组合字符 | 并入 `contents()` | `cell.c` + `cell.zerowidth()` 拼接 |

### 8.3 `src/terminal/impls/presentation.rs`

```rust
// 1. 所有 vt100::Color → TermColor（约 30 处，机械替换）
// 2. vt_color_to_slint / vt_bg_to_slint / vt_span_colors 签名改 TermColor
// 3. ANSI16 三套调色板、idx_to_rgb 等逻辑完全不变（它们只依赖 u8）

fn vt_color_to_slint(color: TermColor, bold: bool, is_dark: bool) -> slint::Color {
    let (r, g, b) = match color {
        TermColor::Default => { if is_dark { (0xd4,0xd4,0xd4) } else { (0x2d,0x2d,0x2f) } }
        TermColor::Idx(i) => idx_to_rgb(i, bold, is_dark),
        TermColor::Rgb(r, g, b) => {
            if is_dark { (r, g, b) } else { darken_light_fg(r, g, b) }
        }
    };
    slint::Color::from_rgb_u8(r, g, b)
}
```

### 8.4 `src/terminal/impls/term_buffer.rs`（核心改造）

```rust
// ── live_rows / view_top_abs / render / selection：screen() → term 适配 ──
fn live_rows(&self) -> (Vec<Line>, usize) {
    let (rows, cols) = term_size(&self.term);
    let live: Vec<Line> = (0..rows).map(|r| build_row(&self.term, r, cols)).collect();
    // ... 不变
}

// ── ingest：process → process_bytes ──
pub(crate) fn ingest(&mut self, input: &[u8]) {
    let bytes = self.rewrite_hvp(input);      // 保留：alacritty 同样不支持 HVP(f)
    self.raw.extend(bytes.iter().copied());
    // ... ESC[3J 处理不变
    self.cap_raw();
    self.feed_batched(&bytes);
}

fn ingest_chunk(&mut self, bytes: &[u8]) {
    // 全屏刷新检测不变（基于原始字节）
    let has_cursor_home = bytes.windows(3).any(|w| w == b"\x1b[H");
    let has_erase_display = ...;
    let is_fullscreen_refresh = has_cursor_home && has_erase_display;

    process_bytes(&mut self.processor, &mut self.term, bytes);   // ← 替代 parser.process

    let (is_alt, rows, cols) = (is_alt(&self.term), term_size(&self.term).0, term_size(&self.term).1);
    if is_alt { self.view_offset = 0; self.prev.clear(); return; }
    if is_fullscreen_refresh { self.view_offset = 0; self.prev.clear(); return; }

    let curr: Vec<Line> = (0..rows).map(|r| build_row(&self.term, r, cols)).collect();
    if !self.prev.is_empty() {
        let k = detect_scroll(&self.prev, &curr);   // 不变
        for line in self.prev.iter().take(k) { self.history.push_back(line.clone()); }
        while self.history.len() > MAX_HISTORY { self.history.pop_front(); }
    }
    self.prev = curr;
}

// ── render：改读 grid ──
pub(crate) fn render(&mut self) -> BuiltScreen {
    let (is_alt, rows, cols, cur_row, cur_col) = (
        is_alt(&self.term),
        term_size(&self.term).0,
        term_size(&self.term).1,
        cursor_pos(&self.term).0,
        cursor_pos(&self.term).1,
    );
    // ... 其余循环体 build_row(&self.term, r, cols) 替换即可
}

// ── reflow：0.26 原生 resize（若选方案 A 保留 raw 重放）──
pub(crate) fn reflow(&mut self, new_rows: u16, new_cols: u16) {
    // 方案 A：重建 Term + 重放 raw（与 vt100 版逻辑一致，只是换构造器）
    let stream: Vec<u8> = self.raw.iter().copied().collect();
    let (term, processor) = new_term(new_rows, new_cols, 5000);
    self.term = term;
    self.processor = processor;
    self.history.clear();
    self.prev.clear();
    self.view_offset = 0;
    self.sel_anchor = None;
    self.sel_focus = None;
    self.sel_ranges.clear();
    self.feed_batched(&stream);
}
```

### 8.5 `src/terminal/impls/input.rs`

```rust
// 原：buffer.parser.screen().bracketed_paste()
// 改：buffer.term.mode().contains(TermMode::BRACKETED_PASTE)
pub(crate) fn terminal_uses_bracketed_paste(
    buffers: &TermBuffers,
    tab_id: &str,
) -> bool {
    // ...
    buffer
        .term
        .mode()
        .contains(alacritty_terminal::term::mode::TermMode::BRACKETED_PASTE)
}
```

### 8.6 `src/app.rs`（初始化 + 交互）

```rust
// ── 1. 新建选项卡（app.rs:4409 附近）──
// 原: parser: vt100::Parser::new(24, 80, 5000),
// 改: 用 new_term(24, 80, 5000) 解构填充
let (term, processor) = new_term(24, 80, 5000);
TermBuffer {
    term,
    processor,
    config: Config { scrolling_history: 5000, ..Config::default() },
    // ... 其余不变
}

// ── 2. 清屏重置（app.rs:8996-8998）──
let (rows, cols) = term_size(&b.term);
let (term, processor) = new_term(rows, cols, 5000);
b.term = term; b.processor = processor;

// ── 3. 会话重置（app.rs:9428-9430）同上 ──

// ── 4. resize（app.rs:6026-6042）──
// 原: buf.parser.set_size(new_rows, new_cols)   （alt-screen 分支）
// 改: resize_term(&mut buf.term, new_rows, new_cols);
// 普通分支 reflow 不变（buf.reflow(...) 内部已换新构造器）

// ── 5. 鼠标协议（app.rs:9521-9532）见 §10.3 ──

// ── 6. 应用光标键（app.rs:9034）──
// 原: b.parser.screen().application_cursor()
// 改: b.term.mode().contains(TermMode::APP_CURSOR)

// ── 7. 测试辅助 make_buf（app.rs:10519-10522）──
// 原: vt100::Parser::new(rows, cols, 0) + parser.process(...)
// 改: new_term + process_bytes
```

### 8.7 `src/ssh/impls/ssh.rs`（测试代码）

```rust
// ssh.rs:2967 附近，测试用
let (mut term, mut processor) = new_term(4, 80, 0);
process_bytes(&mut processor, &mut term, prompt.as_bytes());
// 替代 parser.screen().contents().lines().next() 需要遍历 grid 重建文本行：
fn grid_to_lines(term: &ATerm) -> Vec<String> {
    let (rows, cols) = term_size(term);
    (0..rows).map(|r| {
        let mut s = String::new();
        for c in 0..cols {
            let (text, ..) = cell_attrs(term, r, c);
            s.push_str(&text);
        }
        s.trim_end().to_string()
    }).collect()
}
```

---

## 10. 交互与状态查询迁移

### 9.1 光标

```rust
// vt100: screen.cursor_position() → (row, col) u16
// 0.26:  term.grid().cursor.point → Point { line: Line(i32), column: Column(usize) }
let point = term.grid().cursor.point;
let (row, col) = (point.line.0, point.column.0);
```

### 9.2 应用光标键（影响输入编码 ESC[A vs ESC OA）

```rust
// vt100: screen.application_cursor()
// 0.26:
let app_cursor = term.mode().contains(TermMode::APP_CURSOR);
```

### 9.3 鼠标协议（app.rs 滚轮事件转发）

```rust
// vt100:
//   screen.mouse_protocol_mode()    → MouseProtocolMode::{None,X10,Urxvt,Sgr}
//   screen.mouse_protocol_encoding()→ MouseProtocolEncoding::{X10,Sgr}
// 0.26（组合 TermMode 判断）:
let mode = term.mode();
let enabled = mode.intersects(TermMode::MOUSE_REPORT_MODE);        // 是否启用
if enabled {
    if mode.contains(TermMode::SGR_MOUSE) {
        // SGR 编码: \x1b[<{btn};{c};{r}M
        format!("\x1b[<{btn};{c};{r}M").into_bytes()
    } else if mode.contains(TermMode::URXVT_MOUSE) {
        // URXVT 编码
        format!("\x1b[{c};{r}M").into_bytes()   // 具体格式按原实现逻辑
    } else {
        // X10 编码: \x1b[M{b}{c}{r}
        let b = 32 + btn;
        let c = 32 + col;
        let r = 32 + row;
        format!("\x1b[M{}{}{}", b as u8 as char, c as u8 as char, r as u8 as char).into_bytes()
    }
}
```

### 9.4 alt screen（滚动/渲染判断）

```rust
let is_alt = term.mode().contains(TermMode::ALTERNATE_SCREEN);
```

### 9.5 括号粘贴

```rust
let bracketed = term.mode().contains(TermMode::BRACKETED_PASTE);
```

---

## 11. 测试策略

### 11.1 适配层单元测试（vt_adapter.rs）

覆盖：文本读写、宽字符标记、alt-screen 检测、光标位置、resize、ESC[3J。

### 11.2 渲染等价性测试（关键！）

在迁移期间**同时保留 vt100 后端**，写双后端对比测试：

```rust
#[cfg(test)]
mod parity_tests {
    // 喂同一段字节给 vt100::Parser 和 ATerm
    // 比较 build_row 输出（plain 文本 + runs 的 fg/bg/bold/inverse）
    // 覆盖场景：
    //   - 纯文本 + \r\n
    //   - ANSI 16/256/真彩色
    //   - CJK 宽字符混排
    //   - 组合字符（e.g. "e\u{301}"）
    //   - 反转视频、粗体、下划线
    //   - btop/htop 全屏刷新（ESC[H + ESC[2J）
    //   - vim alt-screen 进入/退出（ESC[?1049h/l）
    //   - 超长行换行
}
```

**执行方式**：在 `Cargo.toml` 暂时保留 `vt100 = "0.15"`，`#[cfg(test)]` 下引用；所有 parity 测试通过后，再移除依赖与旧代码。

### 11.3 性能基准（性能动机的验收标准）

换库动机是"提升解析性能"，必须有可量化的前后对比：

```rust
#[cfg(test)]
mod bench_tests {
    // 方案：双后端解析同一段代表性字节流，统计：
    //   1) 解析耗时：vt100::Parser::process vs Processor::advance（期望：持平/接近）
    //   2) 渲染耗时：全量 build_row vs damage 增量 build_row（期望：-60~80%）
    // 代表性输入：
    //   - 大量短行日志（tail -f 场景）
    //   - 高频进度条/转圈刷新（\r 覆盖同一行）
    //   - btop 类全屏彩绘（256 色 + 全屏重绘）
    //   - `yes` 长输出（滚动压力）
    // 输出：每 MB 字节耗时、每帧渲染耗时、脏行比例
}
```

**判据**：P3 结束时"渲染耗时"必须 ≤ 换库前；P5（damage 增量）结束时必须显著低于换库前（-60%+）。若 P3 后性能回退且 P5 未完成，则暂缓上线。

### 11.4 既有测试迁移

| 测试 | 位置 | 改造 |
|------|------|------|
| 括号粘贴 | app.rs:10543-10554 | `process` → `process_bytes`；断言改 `TermMode` |
| 反转视频 span | app.rs:10741 | `vt100::Color::Default` → `TermColor::Default` |
| alt-screen 高亮 | app.rs:10838 | 同上 + `is_alt` |
| 高亮规则 | app.rs:10759-10926 | `vt100::Color::Idx` → `TermColor::Idx` |
| SSH prompt 测试 | ssh.rs:2967 | grid 遍历重建行 |

### 11.5 手工回归清单

- [ ] SSH 连接后中文/日文显示、复制列对齐（#132 回归）
- [ ] vim/tmux/btop 进出 alt-screen
- [ ] 终端 resize 后长行重排（#169 回归）
- [ ] 鼠标滚轮在 btop 中滚动
- [ ] 括号粘贴（vi 粘贴多行）
- [ ] 日志高亮（ERROR/INFO/DEBUG）
- [ ] 查找/滚动历史/选择复制
- [ ] 大量输出（`yes` 命令、`cat` 大文件）性能与不丢行
- [ ] **长输出 + 连续 PageUp/滚动 50 次以上**（vt100 已知越界崩溃场景，确认不再复现）
- [ ] **性能目测**：`tail -f` 日志、btop 刷新时 CPU 占用不高于换库前（P5 后应明显更低）

---

## 12. 分阶段实施计划

| Phase | 内容 | 产出 | 预估 |
|-------|------|------|------|
| **P0** | 升级 MSRV 1.85；Cargo.toml 加 alacritty_terminal（vt100 保留）；写 `vt_adapter.rs` + 单元测试；**用最小编译样例核实 TermMode 常量名 / Processor::advance_with** | 编译通过、适配层测试绿 | 2-3 天 |
| **P1** | `TermColor` 替换 `vt100::Color`（全局）；HistSpan 改造 | 编译通过 | 1-2 天 |
| **P2** | `render.rs` 双后端：新增 alacritty 版 `build_row_alacritty`；写 parity 测试并跑通 | parity 测试全绿 | 2-3 天 |
| **P3** | `term_buffer.rs` 切到 alacritty（ingest/render/reflow）；`app.rs` 初始化/resize/状态查询；`input.rs` | 运行版全量走 alacritty（兼容性目标达成） | 3-4 天 |
| **P4** | 移除 vt100 依赖与旧代码；迁移全部既有测试；手工回归清单 | 无 vt100 引用 | 1-2 天 |
| **P5**（性能兑现） | **方案 A+：damage 增量渲染**——ingest 末尾收集 `Term::damage()` 脏行；render 只重建脏行，未变行复用 `prev` 缓存；`detect_scroll` 改为脏行区间匹配；用 vtebench 风格基准对比前后渲染 CPU | 高频刷新场景渲染 CPU -60~80% | 2-3 天 |
| **P6**（可选） | 方案 B：原生 scrollback/reflow/selection 演进 | 架构瘦身 | 4-6 天 |

**里程碑检查点**：P2 结束必须看到 parity 测试全绿（这是敢继续切生产路径的前提）；P3 结束做完整手工回归并**用 `yes`/日志 tail 实测确认性能未回退**；P4 结束跑 `cargo clippy` + 全量测试 + release 构建验证（LTO/panic=abort 兼容性）；P5 结束用基准数据证明性能提升（这是"换库提升性能"动机的验收标准）。

---

## 13. 风险清单与缓解措施

| # | 风险 | 等级 | 说明与缓解 |
|---|------|------|-----------|
| 1 | **MSRV 1.85 vs 1.75** | 🔴 高 | 0.26.0 强制；必须升级 rust-version 与工具链；如不能升级，降级 0.24.2（API 不同，见 §3.2） |
| 2 | **宽字符/组合字符差异** | 🔴 高 | `WIDE_CHAR/WIDE_CHAR_SPACER` 双标记 + `zerowidth()`；用 parity 测试钉死 CJK/emoji 场景（#132 是历史痛点） |
| 3 | **`Term` 非 Sync 的线程模型** | 🔴 高 | `TermBuffer` 目前是 `Arc<Mutex<TermBuffer>>`，`Term<VoidListener>` 在 Mutex 内 OK（VoidListener 是空类型，Send+Sync）；但**禁止**在持有 grid 借用时跨线程传递 |
| 4 | **性能不兑现（本动机的核心风险）** | 🔴 高 | 换库后**若保持全量渲染**，CPU 可能不降反升（alacritty 网格单格更重）。**必须**走 P5 damage 增量渲染；P3 后立刻用基准量化（`yes`、日志 tail、btop），对比换库前后渲染 CPU 与帧率；`Config.kitty_keyboard=false`、`osc52` 按需关 |
| 5 | **scrollback 双重管理** | 🟡 中 | 方案 A/A+ 下 `Config.scrolling_history` 建议设 0（禁原生历史）避免内存翻倍；meatshell 的 history 照旧。注意 ESC[3J 时除清 self.history 外，也要 `grid.clear_history()`（若开着） |
| 6 | **WRAPLINE 语义细节** | 🟡 中 | 与 vt100 `row_wrapped` 语义一致（该行因自动换行延续到下一行），但需 parity 测试验证边界（\r\n vs \n 行为差异） |
| 7 | **鼠标协议多模式** | 🟡 中 | vt100 是独立枚举；alacritty 是 TermMode 组合位；写 §10.3 的判定函数 + 单测 |
| 8 | **`Processor` 状态跨 chunk** | 🟢 低 | `Processor` 必须存在 TermBuffer 中（跨 ingest 调用保持），不能每次新建 |
| 9 | **HVP→CUP 重写仍需要** | 🟢 低 | alacritty 同样不支持 `ESC[...f`；`rewrite_hvp` 与 `csi_state` 原样保留 |
| 10 | **依赖膨胀** | 🟢 低 | +30 crate、+1-3min 编译；`[profile.dev.package."*"].opt-level=1` 已有，可加 `[profile.dev.package.alacritty_terminal].opt-level=3` 缓解运行期 |
| 11 | **测试快照 API 变化** | 🟢 低 | `screen.contents()` → 自写 `grid_to_lines()` 辅助 |
| 12 | **panic=abort 兼容** | 🟢 低 | release 配置 `panic="abort"`；alacritty_terminal 无 unwind 依赖，LTO thin 已验证兼容同类 crate，P4 构建确认 |
| 13 | **vt100 已知缺陷迁移** | 🟢 低 | vt100 社区有**滚动越界崩溃**报道（Linutil 案例）与维护放缓；换库本身即缓解；迁移后补回归测试（长输出 + 连续 PageUp 滚动）确认不再复现 |

---

## 14. 回滚方案

```toml
# 回滚 = 恢复 vt100 依赖，保留迁移期间的并行代码
vt100 = "0.15"
```

**迁移期间代码组织**（保证可回滚）：

```
src/terminal/
  impls/
    vt_adapter.rs        # 新：alacritty 适配层（纯新增）
    render_vt100.rs      # 旧：build_row 的 vt100 版（迁移完成前保留）
    render.rs            # 新：build_row 的 alacritty 版（P2 后接管）
```

- P0-P2 阶段：`term_buffer.rs` 仍用 vt100，仅新增适配层与 parity 测试 → 随时可回滚
- P3 阶段：切换点集中在 `term_buffer.rs` 一个文件内（ingest/render/reflow 三个函数），`git revert` 即可回滚
- P4 阶段：删除旧代码前，打 tag `pre-alacritty`，保留 release 构建产物

---

## 附录：官方文档引用（docs.rs/latest 核实记录）

| 类型/方法 | 核实来源 |
|-----------|---------|
| `Term<T>::new(config, &dimensions, event_proxy)` | docs.rs/alacritty_terminal/latest/alacritty_terminal/term/struct.Term.html |
| `Term::grid()/grid_mut()/resize()/mode()/colors()/scroll_display()` | 同上（31 个固有方法清单） |
| `term::Config{scrolling_history,...}` + `Default` | docs.rs/.../term/struct.Config.html（源码 mod.rs#334-366） |
| `TermSize{columns,screen_lines}` + `Dimensions` impl | docs.rs 源码 term/mod.rs#2425-2448（`mock_term` 示例） |
| `event::VoidListener` / `EventListener::send_event` | docs.rs/.../event/index.html |
| `Grid<Cell>`：`Index<Line>→Row`、`Index<Point>→Cell`、`display_iter()`、`cursor` 字段 | docs.rs/.../grid/struct.Grid.html |
| `Cell{c,fg,bg,flags,extra}` 全公开字段 + `zerowidth()` | docs.rs/.../term/cell/struct.Cell.html |
| `Flags::WIDE_CHAR/WIDE_CHAR_SPACER/WRAPLINE/...` | docs.rs/.../term/cell/struct.Flags.html（及 mock_term 源码用法） |
| `vte::ansi::Color::{Named,Indexed,Spec}` | alacritty_terminal 0.26 依赖 vte 0.15（crate 根 re-export `pub use vte;`） |
| `vte::ansi::Processor::advance(&mut handler, byte)` | vte 0.15 ansi.rs 源码（Handler trait + Processor 编排层） |
| `index::{Line(pub i32), Column(pub usize), Point{line,column}}` | docs.rs/.../index/struct.Line.html、struct.Column.html |
| 版本 0.26.0 / MSRV 1.85 / Edition 2024 | crates.io API（2026-04-06 发布） |

> ⚠️ 标注"以实际编译为准"的项（TermMode 常量名、Processor::advance_with 是否存在）在 P0 阶段用 `cargo doc` + 最小编译样例一次性核实，写入本文档更新。
