# meatshell 性能优化专项方案（不换库）

> **编写日期**: 2026-08-01
> **目标**: 仅增强 meatshell 自身性能，**不更换 VT 解析库**
> **核心结论**: 性能瓶颈不在解析器（vt100 底层与 alacritty 同为 vte 状态机），而在**渲染管线的双重全量遍历**——这些优化与是否换库正交，不换库即可获得大部分收益

---

## 目录

1. [结论摘要](#1-结论摘要)
2. [瓶颈分析（代码级证据）](#2-瓶颈分析代码级证据)
3. [优化清单（按收益排序）](#3-优化清单按收益排序)
4. [测量方案（先量化再动手）](#4-测量方案先量化再动手)
5. [与换库的关系](#5-与换库的关系)
6. [实施建议](#6-实施建议)

---

## 1. 结论摘要

| 项目 | 结论 |
|------|------|
| **瓶颈位置** | 不是解析器（vte 表驱动状态机，每字节 O(1)），而是**渲染管线的全量遍历** |
| **第一大浪费** | `ingest()` 每批（最多 32 批/64KB）都全量 `build_row` 所有行 + `detect_scroll` 全行 diff；`render()` 每帧又全量一遍 → **同一行一帧内被构建 2+ 次** |
| **第二大浪费** | 每帧全量跑 `highlight_plain_output`（正则）+ `render_term_span`（grapheme 切分）+ 全量重建 `VecModel` + Slint 全量重建场景树 |
| **优化空间** | 常规追加输出场景（tail、日志、命令输出），**渲染 CPU 可降 70-90%**，且不换库 |
| **核心思路** | **行级增量（damage）**：只重建"变化行"的 span/高亮/模型，未变行复用缓存——与换库后的 `Term::damage()` 是同一思路，将来换库无缝衔接 |
| **工作量** | 优化 1+2（核心）：2-4 天；全套：1-2 周 |

---

## 2. 瓶颈分析（代码级证据）

### 2.1 数据流全景

```
事件泵线程                                    UI 线程
─────────────                                ─────────
ssh/pty 输出 (64KB 分批)
    │
    ▼
ingest_terminal_output()  app.rs:46
    │
    ▼
TermBuffer::ingest()      term_buffer.rs:231
    │  rewrite_hvp → raw 追加 → ESC[3J 检测
    ▼
feed_batched()            term_buffer.rs:265   ← 按 \n 拆批，每批 ≤ 半屏行数
    │  （64KB 可拆成 20-30 批）
    ▼
ingest_chunk()            term_buffer.rs:366
    │  process(bytes)
    │  build_row() × rows  ──────────────┐   ← ① ingest 阶段全量行构建
    │  detect_scroll(prev, curr)         │      （每批一次！）
    ▼                                    ▼
render_gate.request()（合并 + 33ms 限速）
    │
    ▼
do_tab_render_flush()     app.rs:316
    │
    ▼
rebuild_tab_display()     app.rs:6049
    │  buf.render()        term_buffer.rs:416
    │    build_row() × rows  ────────────┘   ← ② render 阶段又全量一遍
    │    highlight_plain_output()（每帧跑正则）
    │    render_term_span() × 所有 runs（grapheme 切分）
    │  compute_find_matches()（每帧全文本跑正则）
    │  selection_rects_visible()
    ▼
VecModel 全量重建 spans → set_terminal_row → request_redraw
    │
    ▼
terminal_view.slint:736  for span in root.spans : Rectangle { Text }
    └── 每个 span 一个场景树节点（几百~几千个），全帧更新
```

### 2.2 浪费量化

| # | 浪费点 | 位置 | 量化 | 后果 |
|---|--------|------|------|------|
| ① | **ingest 每批全量 build_row** | `ingest_chunk()`:398-400 | 64KB 输出 ≈ 20-30 批 × rows(24-40) 行 | 常规输出时 600-1200 次/帧行构建，实际变化 <30 行 |
| ② | **render 再全量一遍** | `render()`:430 | rows 行 × cols 列 | 与 ① 重复；静态画面也全量 |
| ③ | **detect_scroll 全行 diff** | `render.rs`:137 | prev vs curr 全行字符串比较 | 每批一次，O(rows × 行文本长度) |
| ④ | **高亮正则每帧全跑** | `highlight_plain_output()` | 每帧对每行跑 regex（含自定义规则） | 日志输出场景是大头 |
| ⑤ | **find 匹配每帧全跑** | `compute_find_matches()` | 每帧对全部 displayed_text 跑正则 | 查找开启时显著 |
| ⑥ | **VecModel 全量重建** | `rebuild_tab_display()`:6060 | new VecModel(b.spans) | Slint 场景树全量 diff |
| ⑦ | **Slint 节点爆炸** | `terminal_view.slint`:736 | 每 span 一个 Rectangle+Text | 全屏彩绘（btop）时 1000+ 节点 |

### 2.3 关键洞察

- **同一行一帧内被构建 ≥2 次**（①+②），这是最大的确定性浪费
- 终端输出 99% 是**追加式**（光标附近写入），只有 btop/vim 才全屏重绘
- 所有优化都可归纳为一句话：**只重建变化行，其余行走缓存**

---

## 3. 优化清单（按收益排序）

### 🥇 优化 1：ingest 阶段只 diff 尾部窗口（收益最大，1-2 天）

**现状**：`ingest_chunk` 每批全量 `build_row` 构造 `curr`，再全量 `detect_scroll`。

**改法**：终端输出是追加式的，滚动只发生在**底部窗口**。只对"最后 N 行"做 diff：

```rust
// term_buffer.rs ingest_chunk 改造（示意）
fn ingest_chunk(&mut self, bytes: &[u8]) {
    // ...全屏刷新/alt-screen 检测不变...

    self.parser.process(bytes);

    // ① 只构建尾部窗口（光标所在行 + 其上一行，共 2 行）参与 diff；
    //    全屏重绘场景（ESC[H+2J 已被上面分支拦截）无需全量。
    let (rows, cols) = self.screen_size();
    let cur_row = self.cursor_row();
    let window_start = cur_row.saturating_sub(1);        // 最多 2 行
    let window_rows = (window_start..rows).map(|r| build_row(s, r, cols)).collect::<Vec<_>>();

    // ② detect_scroll 只在窗口内做（复用 prev 的对应切片）
    if !self.prev.is_empty() {
        let k = detect_scroll(&self.prev_tail, &window_rows);  // 尾窗口 diff
        // 滚动量 k 一定 < 窗口大小；把 prev 的前 k 行推入 history
        for line in self.prev.iter().take(k) { self.history.push_back(line.clone()); }
    }
    self.prev_tail = window_rows;  // 只缓存窗口
}
```

**要点**：
- 滚动必然发生在底部 → 尾部窗口 diff 足够捕获 scroll 量（`\n` 使上一行上移，正在窗口内）
- 全屏刷新（`ESC[H`+`ESC[2J`）已在 `ingest_chunk` 开头的分支里跳过历史捕获，不受影响
- **收益**：常规输出 build_row 从 rows(24-40) → 2 行，**ingest 路径 CPU 降 ~90%**

> ⚠️ 需要验证：btop/htop 非 alt-screen 全屏刷新模式、`clear` 命令、快速滚屏（`yes`）下的正确性——写针对性测试（见 §4）。

### 🥈 优化 2：render 行级增量（次高收益，1-2 天）

**现状**：`render()` 每帧全量 build_row + 高亮 + span 生成。

**改法**：为每行缓存"渲染产物"，只重建变化行：

```rust
// types.rs 新增缓存字段
pub(crate) struct TermBuffer {
    // ...现有字段...
    pub(crate) rendered: Vec<Option<RenderedLine>>,  // 按行缓存（见下）
}

// 缓存的行级渲染产物（替代每帧重建）
#[derive(Clone)]
pub(crate) struct RenderedLine {
    pub(crate) plain: String,          // 用于 find / 显示
    pub(crate) runs: Vec<HistSpan>,    // 高亮后的 run（含 emoji 拆分后的 TermSpan 来源）
}

// render() 改造（示意）
pub(crate) fn render(&mut self) -> BuiltScreen {
    // ...收集 is_alt/rows/cols/cursor...
    let live: Vec<Line> = (0..rows).map(|r| build_row(s, r, cols)).collect();

    // 行级 diff：plain 文本变了才重建 run + 高亮 + span
    for r in 0..rows {
        let key = &live[r].0;  // 该行 plain 文本（构建快，仅 String）
        let cached = &mut self.rendered[r];
        let line_is_new = cached.as_ref().map_or(true, |c| c.plain != *key);
        if line_is_new {
            let runs = highlight_plain_output(live[r].1.clone(), preset, rules);  // 只跑变化行
            *cached = Some(RenderedLine { plain: key.clone(), runs });
        }
        // 从 cached.runs 生成 TermSpan（也可以缓存最终 TermSpan，见优化 4）
    }
}
```

**收益**：静态/低频刷新画面 render 开销 → 近零；高频刷新只重建变化行。

### 🥉 优化 3：高亮结果缓存（0.5-1 天）

`highlight_plain_output` 目前每帧跑正则。配合优化 2，**只在行变化时跑**即可（已并入优化 2 的示意代码）。若想更进一步，可在 `ingest_chunk` 阶段就预生成高亮 run，render 直接读。

### 优化 4：TermSpan 与 VecModel 增量更新（1 天）

**现状**：`rebuild_tab_display` 每帧 `new VecModel(b.spans)`，全量替换。

**改法**：
- 缓存每行的 `Vec<TermSpan>`，行变化时才重新生成（优化 2 的延伸）
- Slint 侧用 `VecModel::set_row_data` + `notify_row_data_changed` 增量更新，而非整表替换：

```rust
// app.rs rebuild_tab_display 改造（示意）
let model = spans_model_for_tab(tab_id);  // 复用已有 VecModel，不每次 new
for (row, spans) in changed_rows {
    model.set_row_data(row, spans);       // 只更新变化行
}
```

> 前提：terminal_view.slint 的 `for span in root.spans` 需要改为按行分组的模型（`VecModel<VecModel<TermSpan>>`）或保持平铺但按 row 过滤。若改动大，可先只做"行变化才生成新 VecModel"（仍全量替换但省了生成成本）。

### 优化 5：find 匹配惰性化（0.5 天）

**现状**：`compute_find_matches` 每帧对全部 `displayed_text` 跑正则。

**改法**：
- 只在 `find_query` 变化 或 `displayed_text` 内容变化时重算
- 增量：只对变化行重算匹配区间（`displayed_text` 每行一个 String，天然按行隔离）

```rust
fn compute_find_matches_cached(
    buf: &mut TermBuffer,
    query: &str,
) -> Vec<TermMatch> {
    // 缓存 (query, 已算行版本)；行版本变化才重算该行
    if buf.find_query != query { buf.find_cache.clear(); buf.find_query = query.to_string(); }
    // 只对 rendered 行版本 > 缓存版本 的行重算
}
```

### 优化 6：事件泵合并窗口微调（0.5 天，低优先级）

`INGEST_FRAME_BUDGET = 64KB` + render_gate 33ms 已合理。可微调：
- 事件泵已把连续 Output 合并到 64KB；可确认锁竞争（`Arc<Mutex<TermBuffer>>`）不是瓶颈——用 §4 测量
- 若锁竞争明显，可改为"解析线程直接 ingest，UI 线程只读快照"的 split-lock 模式（改动大，暂缓）

### 优化 7：span 合并与 emoji 缓存（0.5 天，低优先级）

- `render_term_span` 已按 grapheme 合并 + twemoji 缓存（`TWEMOJI_CACHE`），已良好
- 可加：相邻 run 同色合并（`build_row` 已做 run 合并，`highlight_plain_output` 拆分后未回并——可在高亮后追加一次合并）

### 优化 8：Slint 场景树减负（1-2 天，UI 侧）

**现状**：`terminal_view.slint:736` 每 span 一个 `Rectangle + Text`，全屏彩绘 1000+ 节点。

**改法**（按成本递增）：
1. 低：增加"空白/默认样式 span 合并"（优化 7 的 UI 配合）
2. 中：把同一行的多个 span 合并为一个 Text（用 `text` 拼接 + 分段着色需要 Slint 不支持，放弃）
3. 高：改用 **Text 的 rich-text** 或自定义渲染（Slint 支持有限，评估成本）

> 建议先做 1；若 btop 类全屏彩绘仍卡，再评估 3。

---

## 4. 测量方案（先量化再动手）

### 4.1 快速基准（必做）

在 `ingest` / `render` 关键路径加计时（或写 bench 测试）：

```rust
#[cfg(test)]
mod perf_tests {
    // 构造代表性输入，分别测：
    //   A. 大量短行日志（tail -f 场景）： 10MB "INFO ...\n"
    //   B. 高频进度条（\r 覆盖）：         100k 次 "\rprogress: 12%"
    //   C. btop 类全屏彩绘：              1000 帧 256 色 + ESC[H + ESC[2J
    //   D. `yes` 长输出（滚动压力）：      10MB "y\n"
    // 指标：
    //   - ingest 总耗时 / 每 MB 耗时
    //   - render 单帧耗时（全量 vs 优化后）
    //   - 变化行数 vs 总行数（脏行比例）
}
```

优化前先跑一轮记录基线，优化后对比。**判据**：方案 A/B 场景（最日常）render 单帧耗时降 70%+。

### 4.2 运行时采样

- Windows：Windows Performance Analyzer 或 `cargo-flamegraph`（需 nightly/perf）
- 简易法：`tracing` 已接入（`logging/`），在 `ingest`/`render`/`compute_find_matches` 加 `debug` 级计时 span，release 构建下临时打开观察
- 关注：`Arc<Mutex<TermBuffer>>` 锁等待时间（若 >10% 再考虑优化 6）

### 4.3 回归测试（优化后必跑）

- 现有渲染等价性测试（若有）全绿
- 长输出 + 连续滚动（vt100 已知越界场景）不崩溃、不丢行
- btop / vim alt-screen / `clear` / `yes` 手工验证

---

## 5. 与换库的关系

| 问题 | 回答 |
|------|------|
| 换库能自动获得这些优化吗？ | **不能**。换 alacritty 后渲染管线依旧全量遍历，必须做同样的增量改造 |
| 不换库做这些优化，将来换库会白做吗？ | **不会**。优化 1/2 的"行级 damage"思路与 alacritty `Term::damage()` 完全一致，将来换库时把自研 diff 换成 `Term::damage()` 即可，改造量反而更小 |
| 那到底换不换库？ | **性能目标 → 不换库就能达成**（本文档）；**兼容性目标 → 才需要换库**（见 migration plan）。两者独立决策 |
| 最优路径 | 先做本文档优化 1+2（2-4 天），量化收益；若仍不满足性能或需要兼容性，再评估换库 |

---

## 6. 实施建议

### 推荐节奏

| 步骤 | 内容 | 时长 |
|------|------|------|
| 1 | 跑 §4.1 基线基准，量化当前瓶颈 | 0.5 天 |
| 2 | **优化 1**（ingest 尾部窗口 diff） | 1-2 天 |
| 3 | **优化 2+3**（render 行级增量 + 高亮缓存） | 1-2 天 |
| 4 | 优化 4/5（VecModel 增量 + find 惰性） | 1-1.5 天 |
| 5 | 回归测试 + 重新跑基准，对比收益 | 0.5 天 |
| 6 | （按需）优化 7/8 或换库评估 | 可选 |

### 代码组织建议

- 新增 `src/terminal/impls/damage.rs`（或并入 term_buffer.rs）：集中"行级变化追踪"逻辑，与解析器解耦——将来换库时该模块接口不变，内部实现从"自研 diff"切到 `Term::damage()`
- 保持 `build_row` 签名不变（渲染等价性测试可复用）

### 注意事项

1. **先测量后优化**：不要凭直觉改；优化 1 前先确认 ingest 确实是热点
2. **全屏刷新场景单独测**：btop 非 alt-screen 模式、`clear`、vim 退出——这些是"全量重建"的合法场景，增量逻辑必须能识别并回退全量
3. **缓存失效要保守**：`ESC[3J`（清历史）、resize、alt-screen 切换时必须清空 `rendered` 缓存，否则出现残影
4. **与 find/selection 的联动**：`displayed_text` 来自 render，增量后必须保证每帧仍完整更新（find 依赖它）
