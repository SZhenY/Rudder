# 已知问题：MAC版本 SSH 会话存在时，非终端页触摸板滚动卡顿

- **状态**：未解决（严重）
- **回归版本**：0.8.0-ui-beta1（新界面外壳重写之后引入）
- **记录日期**：2026-10-11
- **现场保留**：备份分支 `backup/perf-investigation-20261011`

## 现象

- 打开任意 SSH 会话后（**包括完全空闲、远端无任何输出的会话**），切到设置页 /
  主机页等非终端页，用触摸板滚动出现**周期性卡顿**：滚动事件流被切断，表现为
  「像一直在点击」「滑一下停一两下，然后惯性继续」。
- **关闭全部会话 → 立刻恢复流畅**。同一窗口内有无会话是唯一开关。
- 0.8.0（旧界面外壳）无此问题。
- 与渲染后端无关：`femtovg-wgpu`（GPU）与 `winit-software`（软件）都卡。

## 已实证排除的原因

以下每一项都有实验或数据支撑，重构时**不必再怀疑**：

| #   | 排除项                                                       | 方法与证据                                                                                     |
| --- | ------------------------------------------------------------ | ---------------------------------------------------------------------------------------------- |
| 1   | UI 线程计算阻塞                                              | 心跳探针：UI 线程稳定 58–61 tick/s（理想 62），全程无一次明显停顿                              |
| 2   | Rust 侧每秒任务（`refresh_sidebar` / 事件处理 / 终端重建）   | 计时探针：无一次超过 2–3ms 阈值；按页面过滤后仍卡                                              |
| 3   | CPU 繁忙 / 锁竞争                                            | `sample` 抓栈：main 线程 **90% 时间在 `mach_msg2_trap`（空闲等待）**，渲染仅 4%，无锁等待热点 |
| 4   | 元素树规模（终端网格 1000+ 元素 / 右面板标签页 / 底栏）      | 用 `if` 把网格与右面板内容整体移出元素树后仍卡                                                 |
| 5   | 底部状态栏每秒写入（`net_bot_*` 字符串 + `history` 模型）    | 注释掉全部底栏每秒写入后仍卡                                                                   |
| 6   | 渲染后端                                                     | GPU 与软件两条路径都卡                                                                         |
| 7   | Flickable 滚动实现                                           | 设置页换成原生 `ScrollView` 仍卡                                                               |
| 8   | winit 鼠标事件缓冲机制（`buffer_mouse_move`）                | 关闭后仍卡；且断流时同一时段 move 事件数量**翻倍**（疑似事件合并被反复破坏）                   |
| 9   | IME / 标题栏 / 磨砂（`frost` 仅是半透明色）/ 指针按下        | 探针与最小复现均无关                                                                           |

## 已确认的事实

1. **回归区间锁定**：`git bisect` 确定 0.8.0（`6abd873`，2026-09-26）是好的，
   `7349e3b`（2026-09-27 末，新外壳骨架完成）就已经坏。回归随**新界面外壳重写**
   引入（9/26 → 9/28，88 个提交，±8800 行）。
2. 卡顿与「输出」无关（空闲会话也卡），只与「会话存在」强相关。
3. main 线程 CPU 全程空闲——**这不是计算瓶颈，是事件流被打断**：触摸板滚动事件
   序列被周期性切断。
4. **鼠标事件量常态翻倍**（2026-10-11 实测确认）：打开 SSH 会话后，应用收到的
   鼠标事件数量是无会话时的 **2 倍**——不是断流瞬间才翻倍，而是只要会话存在，
   事件流就整体翻倍。强烈指向 **macOS 的事件合并（NSEvent coalescing）被会话
   存在破坏**：正常时高频触摸板 move 每帧合并成一个；合并失效后每个原始事件都
   投递，滚动相位被打断，表现即断流卡顿。

## 剩余嫌疑（后续重构方向）

- **会话线程与主事件循环的交互方式**（与事实 4 相互印证）：SSH/PTY 线程经
  channel + winit 唤醒主循环，可能破坏了 AppKit 对滚动事件的合并（NSEvent
  coalescing）或 phase 追踪；排查方向是找出「会话存在」改变了哪条事件注册 /
  订阅路径（NSEvent 监听、响应链、辅助功能观察者等）。
- **CVDisplayLink / 帧调度被会话存在所干扰**。
- 重构建议：新外壳下让「会话存在」与「UI 事件路径」完全解耦后再验证；或对照旧
  外壳逐组件移植，找出结构性差异。

## 2026-10-12 修复尝试①：压低采样唤醒的写入成本 —— 无效（已实测）

**改法**（3 处，134 行）：

1. `refresh_active_term` 全量按需写：15+ 个属性改为「值/内容没变就不写」
   （模型类先比身份 —— `ModelRc` 的 `PartialEq` 即 Rc 指针比较；`crumbs` 此前
   **每 tick 新建 VecModel**、身份必变，是「采样唤醒 → 实质重绘」的最大单一
   来源，改为逐项内容比较）；
2. `net_bot_up/down` 字符串按需写（空闲时上下行都是 0 B/s，不再每 tick 打脏标记）；
3. 面板 `match status`（240 行全量重建）按 `shell_page == 1` 跳过，非终端页
   不再为看不见的面板元素做全量模型重建。

**结果**：用户实测**仍卡**。

**推论（本次排查最重要的转折）**：卡顿与「唤醒后写了多少」**无关**，与
「唤醒本身」有关。写入侧优化到此全部无效——此前的网格/右面板条件化、底栏
写入禁用、ScrollView、winit 鼠标缓冲关闭，加上本次三处，方向一致地失败。
说明把主循环从 macOS `nextEvent` 长等中周期性拽出的**不是重绘成本，而是
周期性 wakeup 本身**：每 750ms（远端采样）/ 每 1Hz（本机 sampler）一次
`invoke_from_event_loop`（→ winit `send_event` → 唤醒 RunLoop）就重置一次
AppKit 的 NSEvent 合并窗口，把高频率触摸板 move 切成小批——这也直接解释了
「事件量 2×」：wakeup 的处理路径会把 pending 的 move 提前 flush 一小批，
随后正常的合并流继续，两股叠加。

**下一步方向**：抑制**非终端页上的周期性唤醒本身**（在投递侧过滤，让
`invoke` 根本不发生，而不是在 UI 回调里少写属性），并排查 SFTP pump 的
sweep 是否为更高频的唤醒源。

## 2026-10-12 决定性实验：独立设置窗口不卡 —— 根因确认

**实验**（`ui/settings_probe.slint`，临时）：侧栏点「设置」→ 弹出**独立 root
窗口**（内含真实 PageSettings，属性默认值），主窗口留在终端页。用户在独立窗口
里滚动：**完全流畅**（对照：同一进程里主窗口内嵌的设置页滚动仍卡）。

**根因确认**：卡顿来自**渲染上下文共享**——主窗口设置页滚动的每一帧，Slint 的
`PartialRenderer` 都要为**整棵窗口树**（侧栏 Rail + 标题栏 + 页面容器/切页动画层
+ 设置页 + 终端舞台结构 + 右面板外壳 + 主题派生链）付脏区计算 + 布局 + 呈现
成本；独立窗口的滚动只重绘设置窗口自己的小树，主窗口根本不渲染。这与此前全部
事实自洽：CPU 不忙（不是算不动，是每帧固定开销把帧节奏拖离 vsync）、事件量
2×（滚动帧与主窗口事件路径高频交织）、空闲会话也卡（树结构与数据无关）、关
会话就好（树变轻）、而所有"减少写入/减少唤醒"的修复无效（动的都不是这条链）。

**待解的新问题**：用户的新 UI 设计是**沉浸式单窗口**（侧栏 + 页面容器），不接受
独立窗口形态。而单窗口内的元素级优化已穷尽（网格 / 右面板内容 / TerminalStage /
StatusBar 全部条件化后仍卡）。因此下一步是拆解单窗口内**剩余**的每帧成本：
侧栏 Rail、标题栏、切页动画层、主题派生链 —— 找出独立窗口省掉的那部分。

## 修复方案 A（已选定）：无边框贴靠窗口 —— 详细设计

### 一句话（通俗版）

现在的设置页像是**挂在主窗口这面“电视墙”上的一块面板**——你一滚它，整面墙
（终端、工具面板、状态数据那棵大树）都得跟着重新画一遍，于是就卡。

方案 A 是把这块面板**摘下来，变成一台紧挨着电视墙的独立小屏幕**：小屏幕自己
滚、自己画，电视墙原地不动。用户看到的画面几乎没变（还是侧栏右边那片内容），
但滚动不再牵动整棵大树。这就是上面决定性实验里“独立窗口不卡”的由来——我们
把那份收益保留，同时把外观做回沉浸式单窗口的样子。

### 窗口长什么样（如何做到“看起来没变”）

- `no-frame: true`：没有标题栏、没有边框、没有阴影，背景色与主窗口内容区一致；
- 位置和尺寸**精确等于主窗口的“内容区”**（侧栏右缘 → 窗口右缘，标题栏下沿 →
  底栏上沿），主窗口一动它就动；
- 侧栏露在贴靠窗口左边，照常显示、照常可以点（点侧栏切页面时，贴靠窗口收起）；
- 主题（壁纸 / 深浅色 / 强调色）是全局单例，两边自动一致，不需要同步。

### 交互流程

| 用户动作 | 主窗口 | 贴靠窗口 |
| --- | --- | --- |
| 点侧栏「设置」 | `shell-page` 拉回 1（终端页在背后待命） | 出现，内容 = 设置页 |
| 点侧栏「主机」 | 同上 | 出现，内容 = 主机页 |
| 点侧栏「终端」 | 切到终端页 | 隐藏（不销毁，重开直接 show） |
| 关闭贴靠窗口（红灯 / Esc） | 无感 | 隐藏不销毁（`CloseRequestResponse::HideWindow`） |
| 拖动 / 缩放主窗口 | — | 每帧跟随，保持贴合 |

一个窗口装两页内容（内部 `if` 切 Settings/Hosts），不做两个窗口——管理简单。

### 数据怎么同步（唯一需要仔细的部分）

主窗口现在有一批**窗口级属性**（scrollback、follow-cd、hide-partitions……
约 30 个设置值 + 主机列表），设置页读它们、写也是写回它们。贴靠窗口是另一个
窗口，这些属性要各有一份：

1. **创建 / 每次显示前**：从主窗口复制一份过去（`sw.set_x(m.get_x())` 逐项）。
   旧外壳的 `settings_window.rs::sync_settings_reflected` 就是干这个的，可直接
   参考；
2. **用户在贴靠窗口里改动**：callback 里**双写**——贴靠窗口自己一份（界面立竿
   见影）+ 主窗口一份（终端、状态栏、面板都读主窗口的属性）；
3. **反向**：主窗口侧若发生变化（重连、配置重载），显示贴靠窗口前的那次复制
   会把它带过来，不需要实时反向同步；
4. 主题不同步（全局单例）。

### 分阶段实施

- **Phase 1（验证，地基已打好）**：把现有 `settings_probe.slint` 升级为贴靠版
  —— `no-frame` + 主窗口内容区定位 + move/resize 跟随 + 首帧处理
  （`request_first_frame`，proc 窗口已证明 macOS 上第二个窗口不主动请求就永远
  白屏）。**验收标准：无边框、贴合、滚动手感与独立实验一致、侧栏可点可切。**
- **Phase 2**：主机页进同一个贴靠窗口（`if` 切内容），启动流程改为“启动即贴靠
  显示主机页”；
- **Phase 3**：完整数据同步——约 30 个设置项 callback 双写 + 主机列表模型；
  删掉主窗口内的 `page_settings` / `page_hosts` 死代码；
- **Phase 4（打磨）**：贴靠窗口的出现 / 收起动效（位置滑入，Rust 定时器驱动机
  `set_position`）、焦点管理（贴靠窗口活跃时侧栏的失焦样式处理）、Esc 关闭。

### 风险与对策

| 风险 | 对策 |
| --- | --- |
| 快速拖动主窗口时贴靠窗口跟不上 | 主窗口 position/size changed 里同步 set；macOS 事件频率足够；兜底：最小化/全屏时贴靠窗口一起隐藏 |
| 点击贴靠窗口后主窗口“失焦变暗”，侧栏看着像不能点 | 实际可点（点侧栏会激活主窗口并收起贴靠窗口）；如需更完美，贴靠窗口获焦时抑制主窗口失焦样式 |
| 多显示器 / 屏幕缩放变化 | 贴靠窗口跟随主窗口同一 screen 的逻辑坐标；screen 参数变化时重新对齐 |
| 滚动事件“穿透”到主窗口 | 贴靠窗口只覆盖内容区，侧栏区的滚动事件天然归主窗口，这正是想要的 |
| 两处状态不一致（双写出漏） | Phase 3 给每个同步项配一条注释；主窗口属性是唯一真源，贴靠窗口显示前全量复制兜底 |

### 已就绪的资产

- `ui/settings_probe.slint`：验证版独立窗口（内含真实 PageSettings），升级为
  贴靠版的起点；
- 旧外壳 `settings_window.rs`（0.8.0）：完整的窗口管理模式（静态弱引用保活、
  theme 同步、值反射、打开/关闭），迁移时的主要参考；
- `request_first_frame` + 0/50ms 二次请求：macOS 第二个窗口首帧的现成解法
  （proc 窗口在用）。

### chrome 层拆解结论（2026-10-12）

侧栏 `rail.slint`（562 行）与标题栏 `titlebar.slint`（85 行）的实际每帧成本都很小：
Rail ≈ 20 个静态元素 + 2 个条件性动画（background/border-color 过渡、折叠进度），
AppTitleBar ≈ 10 个元素；主题的 `wp-*` 是 Rust 算好 set 进来的 in-out 属性、不是
每帧从图像派生。**元素数量与遍历成本都不是卡与不卡的鸿沟** —— 独立窗口生效的
唯一原因是**渲染 surface 隔离**：滚动只发生在那一个窗口的脏区里，主窗口根本不
参与。单窗口内在保持设计的前提下无法复现这个隔离（一个 Slint 窗口 = 一个
surface），可行方向只剩「无边框贴靠窗口」这类形态妥协，或转向帧调度层深挖。

## 方案 A 执行手册（施工级）

> 面向实施者：每阶段给出**文件级改动、代码骨架、验收清单**。几何一律用 logical
> 坐标（slint 内部处理 DPI）；主窗口侧栏宽度 `rail-w` 与内容区顶 `shell-top` 需要
> 从 `app_shell.slint` 的私有 property 提升为 in-out（否则 Rust 读不到）。

### Phase 1：贴靠版验证窗口

**1.1 `ui/settings_probe.slint`**（在现有验证版上改）：

```slint
export component SettingsProbeWindow inherits Window {
    no-frame: true;                      // 无标题栏/边框/阴影，外观与主窗口内容区一致
    background: Theme.window-base;       // 主窗口内容区同色；用户有壁纸时与壁纸衔接
    default-font-family: Theme.ui-font-family;
    // 尺寸/位置由 Rust set（要跟随主窗口），slint 侧不绑死
    PageSettings { width: 100%; height: 100%; }
}
```

**1.2 `app_shell.slint`**：`rail-w` / `shell-top` 提升为 in-out（供 Rust 读几何）；
`changed shell-page` 的 page 4 拦截已就位（验证版）。

**1.3 `src/app.rs`**：贴靠同步 + 显示管理：

```rust
// 内容区几何：侧栏右缘 → 窗口右缘，标题栏下沿 → 窗口底（底栏只在终端页显示，
// 与贴靠窗口不同时出现，所以直接算到窗口底）。
fn content_rect(m: &AppWindow) -> (slint::LogicalPosition, slint::LogicalSize) {
    let wp = m.window().position();          // 主窗口屏幕位置（logical）
    let ws = m.window().size();              // 主窗口尺寸（logical）
    let rail = m.get_rail_w();               // in-out 化后才能读
    let top = m.get_shell_top();
    (
        slint::LogicalPosition::new(wp.x + rail, wp.y + top),
        slint::LogicalSize::new(ws.width - rail, ws.height - top),
    )
}

// 16ms 轮询跟随（slint 的 &Window 不暴露 move/resize 事件，只能轮询；
// 只比对、变了才 set，成本可忽略；16ms 迟滞不可感）。
let probe_timer = slint::Timer::default();
{
    let mw = window.as_weak();
    let pw = probe_win.as_weak();
    let mut last: Option<(slint::LogicalPosition, slint::LogicalSize)> = None;
    probe_timer.start(std::time::Duration::from_millis(16), move || {
        let (Some(m), Some(p)) = (mw.upgrade(), pw.upgrade()) else { return };
        if !p.is_visible() { return; }
        let rect = content_rect(&m);
        if last != Some(rect) {
            last = Some(rect);
            p.set_position(rect.0);
            p.set_size(slint::WindowSize::Logical(rect.1));
        }
    });
}

// 点「设置」：同步几何 → show → 请求首帧（macOS 第二个窗口不主动请求就白屏，
// proc 窗口已验证 0/50ms 两次请求的必要性）。
window.on_open_settings_probe(move || {
    let Some(w) = probe_weak.upgrade() else { return };
    if let Some(m) = win_weak.upgrade() {
        let rect = content_rect(&m);
        w.set_position(rect.0);
        w.set_size(rect.1);
    }
    let _ = w.show();
    request_first_frame(w.window());
    let f1 = w.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(50), move || {
        if let Some(w) = f1.upgrade() { request_first_frame(w.window()); }
    });
});
```

**验收清单**：
- [ ] 点「设置」→ 无边框窗口**精确贴合**内容区（无 1px 缝隙/重叠，侧栏分界线对齐）
- [ ] 拖动 / 缩放主窗口 → 贴靠窗口跟随，无可感迟滞
- [ ] 窗口内滚动流畅（= 独立实验的手感）
- [ ] 侧栏照常可点：点「终端」→ 贴靠窗口隐藏、主窗口回终端页
- [ ] 最小化主窗口 → 贴靠窗口不留在屏幕上（轮询里加 `m.is_minimized()` 判断一并隐藏）
- [ ] 关贴靠窗口 → 隐藏不销毁；重点「设置」→ 重新出现且内容保留

### Phase 2：主机页进同一窗口

- `settings_probe.slint` 加 `in property <int> probe-page`，内部
  `if probe-page == 0 { PageHosts {} } else { PageSettings {} }`；
- `changed shell-page` 把 page 0（主机）与 page 4（设置）都拦截，分别设
  `probe-page` 后 show；
- 启动流程：原本启动即主机页（page 0），改为启动后直接显示贴靠主机窗口
  （`on_open_hosts_probe` 的同一路径）；
- **注意**：主机页是启动落点，Phase 1 期间主窗口可**先保留内嵌主机页**（无会话
  时树轻、不卡），Phase 2 再切换——降低单次改动风险。

### Phase 3：完整数据同步 + 清理

**3.1 同步清单的来源**：`page_settings.slint` 的全部 in 属性（约 30 个：scrollback /
flag-osc / update-check / large-scrollback / convert-eol / json-format-output /
hide-partitions / always-ask / follow-cd / collapse-sftp / output-highlight-* 等）
+ `page_hosts.slint` 的主机列表模型。

**3.2 两个方向**：

```rust
// A. 显示前全量复制（主窗口 → 贴靠窗口）：旧外壳 settings_window.rs 的
//    sync_settings_reflected 现成可抄。模型直接 ModelRc::clone（身份不变）。
fn sync_probe_from_main(m: &AppWindow, p: &SettingsProbeWindow) {
    p.set_scrollback(m.get_scrollback());
    p.set_flag_osc(m.get_flag_osc());
    // …逐项
}

// B. 改动时双写（贴靠窗口的 callback）：主窗口属性是唯一真源。
probe_win.on_scrollback_changed(move |v: SharedString| {
    if let (Some(m), Some(p)) = (main_weak.upgrade(), probe_weak.upgrade()) {
        p.set_scrollback(v.clone());   // 贴靠窗口自身：界面立竿见影
        m.set_scrollback(v);           // 主窗口：终端/面板/状态栏都读它
    }
});
```

**3.3 清理**：主窗口移除 `PageSettings` / `PageHosts` 实例（贴靠窗口承载），
`page-shown` 的 0/4 分支随之删除；`app_shell.slint` 里 `page-shown == 0` /
`== 4` 的容器块一并移除。

**验收**：每个设置项在贴靠窗口改 → 主窗口侧立即生效（终端字号、滚回长度等可见
项逐抽查）；主窗口侧重载配置 → 贴靠窗口下次显示拿到新值；全局无 `page_settings`
死代码。

### Phase 4：打磨

- **滑入动效**：Rust 定时器分 6~8 步 `set_position`（从内容区右缘外 → 目标位），
  收起反向；不做透明度（slint Window 无 opacity 属性）；
- **Esc 关闭**：贴靠窗口内 `FocusScope` + `keys` 处理，或窗口级快捷键；
- **失焦样式（已知小瑕疵）**：贴靠窗口获焦时主窗口 `Theme.window-focused` 变 false，
  侧栏会显示失焦变暗——**实际可点**（点侧栏激活主窗口并收起贴靠窗口）。若介意，
  可在轮询里检测贴靠窗口可见时强制主窗口 `window-focused` 绑定为 true（需要把
  `window-focused` 也提升为 Rust 可写）。

### 全局注意事项

- **主题**：全局单例，两边自动一致，**无需任何同步代码**；
- **壁纸衔接**：贴靠窗口背景用 `Theme.window-base`；用户启用壁纸时，主窗口内容区
  也是同一底色 + 壁纸透出——若出现接缝，把壁纸也画进贴靠窗口（`Theme.wallpaper`）；
- **多显示器**：跟随主窗口同一 screen 的 logical 坐标，screen 参数变化时轮询自然
  会重新对齐；
- **性能红线**：贴靠窗口里**不要**放任何 1Hz/750ms 更新的元素（状态栏类），它
  的树必须保持“纯静态 + 滚动”才能继承独立窗口的收益；
- **逃生舱**：若 slint 的 logical 坐标在多显示器 / DPI 场景下不够用，
  `place_process_window`（`resource_ui.rs:398`）示范了 `window().with_winit_window(...)`
  直接操作 winit 窗口（物理坐标）的备选路径。

## 现场保留

- 备份分支 `backup/perf-investigation-20261011`：包含全部实验修复（网格 / 面板
  条件化、按页面过滤、ScrollView 实验、底栏写入禁用）、性能探针（UI 心跳 +
  函数计时）与 bisect 结论，可随时回到该分支复现排查环境。
- 其中 3 个与本次排查无关的独立修复可按需 cherry-pick：

  ```bash
  git cherry-pick c54b798 283e58c d197424
  # c54b798 终端后台时停重建与光标闪烁（正确优化，虽非本问题主因）
  # 283e58c CPU 卡片数值改到进度条位置（功能需求）
  # d197424 自更新测试样例与 release.yml 对齐（测试修复）
  ```

## 2026-10-12 补充：源码级架构对比 → 根因收敛与修复方向

### 架构对比：0.8.0（旧外壳）vs 新外壳 —— Rust 侧唤醒链完全相同
对 `6abd873`（旧外壳，好）与 `7349e3b`（新外壳，坏）逐行比对「会话 → UI」链路，
结论：**新外壳没有新增任何 Rust 侧高频/持续唤醒源**，以下各项新旧一致：

| 唤醒源                                   | 频率        | 旧外壳 | 新外壳 |
| ---------------------------------------- | ----------- | ------ | ------ |
| 远端辅助通道采样（CPU/内存/网络/磁盘）    | 750ms       | 有     | 有（ssh.rs:365-367 同码）|
| 远端进程采样                             | 1500ms      | 有     | 有     |
| 远端系统信息                             | 2500ms      | 有     | 有     |
| 本机 system sampler                      | 1Hz         | 有     | 有（sampler.rs）|
| 每 SSH 会话 Connected 即预开独立 SFTP 连接 + pump 线程 | 常驻 | 有 | 有 |
| shell/SFTP 双 pump（blocking_recv + sweep 合并 + `invoke_from_event_loop` 批量投递）| 事件驱动 | 有 | 有 |
| 终端 render gate 节流                    | 仅在有 Output 时 | 有 | 有 |

- `6abd873:src/app/session_runtime.rs` 与当前逐行几乎一致（连行号都对得上）。
- 静态排除：新外壳无新增高频 Timer（`app_shell.slint` 的 `page-swap-timer` 切页跑
  一次即停；动画全是有界过渡，无 infinite）；`render_tickets`/`render_gate` 仅在
  终端有 Output 时激活（idle 会话不触发）。
- 结论：差异 100% 落在 UI 层（.slint 外壳重写 + 采样数据消费面扩大）。这也解释了
  为何「关 buffer_mouse_move / 按页面过滤 / 网格与右面板条件化 / 禁底栏写入」均无效
  —— 动的都不是这条链。

### 根因（最可能，待动态确认）
新外壳把采样数据消费面大幅铺开：`active_state.rs`（新增）把活动标签的
tunnels/SFTP/统计提升成一大票窗口级属性，新增右面板 `right_panel.slint`，CPU 趋势 /
sparkline / 状态栏大量新绑定。于是每次 1Hz / 750ms 采样唤醒都触发**实质重绘**，把主
循环从 macOS `nextEvent` 长等中周期性拽出 → AppKit 的 NSEvent coalescing 窗口被切成
≤ 采样间隔 → 触摸板高频 move 无法合并、逐个投递 → 事件量 2×、滚动相位切断。与三条实测
事实（UI 60 tick/s、main 90% 空等 `mach_msg2_trap`、事件量 2×）自洽。

旧外壳静止时无持续帧请求，低频采样唤醒被 Slint 的 Wait 模式「吸收」（唤醒 → 单帧 →
回长等），合并窗口保持长 → 不卡。

### 为什么仅 macOS 复现（Windows 无恙）
Rudder 无任何自建消息循环 / 输入合并（无 `NSEvent` / `objc2` / `WindowProc` /
`GetMessage` / `RegisterRawInput`），输入全走 Slint/winit 默认路径，
`invoke_from_event_loop` 三平台一致。差异纯在 OS 输入架构：
- **macOS**：高频触摸板 move 的合并在**应用事件循环层**（AppKit
  `nextEventMatchingMask` 的 coalescing，依赖主循环长等），且触摸板滚动 = 连续
  NSEventPhase + momentum 流 → 采样唤醒切碎合并窗口即断流。
- **Windows**：鼠标/滚轮合并在**驱动/HID 层**（或离散入队），滚动 = 离散 WM_MOUSEWHEEL /
  OS 合成后的 WM_POINTER；应用消息循环没有「可被破坏的合并窗口」，唤醒只是多空转几圈，
  输入流不受影响。

### 修复方向（待 A/B 验证后实施）
1. 给远端辅助通道采样加**页面/焦点感知的 back off**（本机 sampler 已有 Hidden /
   Background 降频，远端采样 750/1500/2500ms 无条件跑 —— 具体不对称点）。
2. 右面板 / 图表 / 窗口级镜像改**脏标记 + 合帧**，非终端页时 `refresh_sidebar` /
   `refresh_active_term` 跳过图表刷新。
3. A/B 验证：在 `backup/perf-investigation-20261011` 上让非终端页跳过上述刷新，测事件量
   是否 2× → 1×；并用 Instruments Time Profiler 确认 60 tick/s 的帧驱动来源。
