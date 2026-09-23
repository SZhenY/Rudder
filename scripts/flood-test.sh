#!/usr/bin/env bash
# Rudder 终端「流畅性 + 耗时」压测脚本 —— 只用系统自带工具（seq / awk / head / tr / perl）
#
# 用法: bash /tmp/rudder-flood.sh <模式> [数量]
#
#   plain      无色大流量（= seq -f ... 的放大版）              默认 100 万行
#   color      每行 8 色交替（SGR 密集）                        默认 50 万行
#   truecolor  真彩渐变 38;2;R;G;B（SGR 参数最长）              默认 30 万行
#   cjk        宽字符 + emoji + 组合字符（字宽 / 整形）         默认 30 万行
#   tui        全屏重绘 \033[2J\033[H + 彩色网格（≈ htop）      默认 200 帧
#   osc        每行改一次窗口标题（OSC 0）                      默认 20 万行
#   cursor     每行切光标显隐 + 样式（DECTCEM / DECSCUSR）      默认 20 万行
#   cr         \r + 清行 高频重写同一行（进度条式）             默认 50 万次
#   mixed      彩色 + 定期标题/光标 + 宽字符 混合               默认 20 万行
#   longline   单行超长（换行 / 回滚）                          默认 200 万字符
#   all        按顺序把上面 9 种各跑一遍，最后给一张汇总表
#
# ── 每次跑完打印三个数 ───────────────────────────────────────────────
#   纯产生   : 同样内容丢给 /dev/null 的耗时（只有生成器在跑，不经过终端）
#   终端内   : 真的输出到终端的耗时（生成器 + 解析 + 渲染；pty 会反向限速，
#              终端慢时生成器也会被堵住，所以这个数就是"终端吃下这段输出要多久"）
#   终端额外 : 上面两个的差 —— 这就是终端自己的账
# 结果同时追加写入 /tmp/rudder-flood-timing.txt（方便一起贴给我）
#
# ── 环境变量 ─────────────────────────────────────────────────────────
#   NOBASELINE=1   跳过"纯产生"那一次（跑超大流量时省一半时间）
#   FLOOD_SMALL=1  所有默认规模缩到 1/100（快速过一遍用）
#
# 例: bash /tmp/rudder-flood.sh all
#     bash /tmp/rudder-flood.sh plain 3000000
#     FLOOD_SMALL=1 bash /tmp/rudder-flood.sh all
set -u

mode=${1:-plain}
size=${2:-}
script=$0
RESULTS=${FLOOD_RESULTS:-/tmp/rudder-flood-timing.txt}

# ── 各模式的默认规模与计数单位（外层报告与内层产出共用这一张表）──────
case "$mode" in
  plain)     def=1000000; unit=行 ;;
  color)     def=500000;  unit=行 ;;
  truecolor) def=300000;  unit=行 ;;
  cjk)       def=300000;  unit=行 ;;
  osc)       def=200000;  unit=行 ;;
  cursor)    def=200000;  unit=行 ;;
  cr)        def=500000;  unit=次 ;;
  mixed)     def=200000;  unit=行 ;;
  longline)  def=2000000; unit=字符 ;;
  tui)       def=200;     unit=帧 ;;
  all)       def="";      unit= ;;
  *) echo "未知模式: $mode（可用: plain color truecolor cjk tui osc cursor cr mixed longline all）" >&2; exit 1 ;;
esac
if [ "${FLOOD_SMALL:-}" = 1 ] && [ -n "$def" ]; then
  def=$(awk -v d="$def" 'BEGIN{v=int(d/100); if(v<1)v=1; print v}')
fi
n=${size:-$def}

# ── 计时工具（macOS 的 date 没有 %N，用 perl 的 Time::HiRes）──────────
now() { perl -MTime::HiRes=time -e 'printf "%.3f\n", time' 2>/dev/null || python3 -c 'import time;print("%.3f"%time.time())' 2>/dev/null || date +%s; }
elapsed() { awk -v a="$1" -v b="$2" 'BEGIN{printf "%.2f", b-a}'; }
fmt_rate() { awk -v c="$1" -v s="$2" -v u="$3" 'BEGIN{
  if (s<=0) { printf "-"; exit }
  r=c/s;
  if (r>=1e6)      printf "%.1f 百万%s/秒", r/1e6, u;
  else if (r>=1e4) printf "%.0f 万%s/秒", r/1e4, u;
  else             printf "%.0f %s/秒", r, u;
}'; }

# ══════════════════════════════════════════════════════════════════════
#  内层：只负责产出（被外层用 FLOOD_INNER=1 调用，不打印任何计时文字）
# ══════════════════════════════════════════════════════════════════════
if [ "${FLOOD_INNER:-}" != 1 ]; then
  # ── 外层：计时 + 基线 + 报告 ────────────────────────────────────────
  run_one() {  # $1 = 输出目标（/dev/null = 基线；空 = 走终端）→ 结果写入全局 LAST
    local t0 t1
    t0=$(now)
    if [ -n "$1" ]; then
      FLOOD_INNER=1 bash "$script" "$mode" "$n" > "$1"
    else
      FLOOD_INNER=1 bash "$script" "$mode" "$n"
    fi
    t1=$(now)
    LAST=$(elapsed "$t0" "$t1")
  }

  report() {  # $1 模式 $2 数量 $3 单位 $4 基线秒 $5 终端秒
    local base=$4 term=$5 ratio extra
    ratio=$(awk -v b="$base" -v t="$term" 'BEGIN{printf "%.2f", (b>0? t/b : 0)}')
    extra=$(awk -v b="$base" -v t="$term" 'BEGIN{printf "%+.2f", t-b}')
    {
      printf '\033[1;33m  ─────────────────────────────────────────────\033[0m\n'
      printf '   模式     : %s  (%s %s)\n' "$1" "$2" "$3"
      if [ -n "$base" ]; then
        printf '   纯产生   : %6s s   (%s)\n' "$base" "$(fmt_rate "$2" "$base" "$3")"
      fi
      printf '   终端内   : %6s s   (%s)\n' "$term" "$(fmt_rate "$2" "$term" "$3")"
      if [ -n "$base" ]; then
        awk -v b="$base" -v t="$term" -v x="$extra" -v r="$ratio" 'BEGIN{
          if (b<0.05) { printf "   终端额外 : %s s   （基线太短，不足以判定）\n", x; exit }
          if (r<=1.15)      printf "   终端额外 : %s s   (×%.2f)  → 终端跟得上 ✓\n", x, r;
          else if (r<=2.0)  printf "   终端额外 : %s s   (×%.2f)  → 终端略慢\n", x, r;
          else              printf "   终端额外 : %s s   (×%.2f)  → 明显是终端瓶颈 ✗\n", x, r;
        }'
      fi
      printf '\033[1;33m  ─────────────────────────────────────────────\033[0m\n'
    } >&2   # 报告走 stderr：把洪水重定向到文件时也照样看得到
    printf '%s  %-9s n=%-9s base=%-7s term=%-7s ratio=%s\n' \
      "$(date '+%H:%M:%S')" "$1" "$2" "${base:--}" "$term" "$ratio" >> "$RESULTS"
  }

  if [ "$mode" = all ]; then
    : > "$RESULTS"   # 本次全量跑：结果文件先清空，最后直接汇总
    echo "=== 全量压测开始（每段前面有黄色标题；共用本文件 ${RESULTS}）===" >&2
    for m in plain color truecolor cjk osc cursor cr longline tui; do
      printf '\033[1;33m── %s ──\033[0m\n' "$m"
      NOBASELINE=${NOBASELINE:-} bash "$script" "$m"    # 递归：每段自己计时/报告
    done
    echo >&2
    echo "=== 汇总（本次运行）===" >&2
    cat "$RESULTS" >&2
    echo "（完整明细也在这里: ${RESULTS}）" >&2
    exit 0
  fi

  if [ "${NOBASELINE:-}" = 1 ]; then
    base=""
    run_one ""
    term=$LAST
  else
    run_one /dev/null
    base=$LAST
    run_one ""
    term=$LAST
  fi
  report "$mode" "$n" "$unit" "$base" "$term"
  exit 0
fi

# ══════════════════════════════════════════════════════════════════════
#  内层：产出（下面每个分支都只往 stdout 写）
# ══════════════════════════════════════════════════════════════════════
case "$mode" in
  plain)
    seq -f 'line %g abcdefghijklmnopqrstuvwxyz 0123456789' "$n"
    ;;
  color)
    awk -v n="$n" 'BEGIN{for(i=1;i<=n;i++) printf "\033[%dm line %d abcdefghijklmnopqrstuvwxyz 0123456789\033[0m\n", 31+(i%8), i}'
    ;;
  truecolor)
    awk -v n="$n" 'BEGIN{for(i=1;i<=n;i++) printf "\033[38;2;%d;%d;%dmline %d abcdefghijklmnopqrstuvwxyz 0123456789\033[0m\n", i%256, (i*7)%256, (i*13)%256, i}'
    ;;
  cjk)
    awk -v n="$n" 'BEGIN{line="汉字宽字符 mixed ＡＢＣ ｅｍｏｊｉ 👨‍👩‍👧‍👦 e\314\201 tab\tend"; for(i=1;i<=n;i++) printf "%7d %s\n", i, line}'
    ;;
  tui)
    awk -v frames="$n" 'BEGIN{rows=40; cols=120;
      for(f=0;f<frames;f++){
        printf "\033[2J\033[H";
        for(r=0;r<rows;r++){
          for(c=0;c<cols;c++) printf "\033[%dm█", 31+((r+c+f)%7);
          printf "\033[0m\n";
        }
      }
      printf "\033[0m"}'
    ;;
  osc)
    awk -v n="$n" 'BEGIN{for(i=1;i<=n;i++) printf "\033]0;rudder flood %d\007line %d abcdefghijklmnopqrstuvwxyz 0123456789\n", i, i}'
    ;;
  cursor)
    awk -v n="$n" 'BEGIN{for(i=1;i<=n;i++){printf "\033[?25l\033[%d q line %d abcdefghijklmnopqrstuvwxyz 0123456789\033[?25h\n", 1+(i%6), i}}'
    ;;
  cr)
    awk -v n="$n" 'BEGIN{for(i=0;i<=n;i++) printf "\r\033[K进度 %d%% %s", i, "=================================================="; print ""}'
    ;;
  mixed)
    awk -v n="$n" 'BEGIN{
      for(i=1;i<=n;i++){
        if(i%50==0)  printf "\033]0;mixed %d\007", i;
        if(i%100==0) printf "\033[?25l\033[3 q";
        printf "\033[%dm%7d 汉字 mixed ＡＢＣ 👨‍👩‍👧‍👦 abcdefghijklmnopqrstuvwxyz 0123456789\033[0m\n", 31+(i%8), i;
        if(i%100==0) printf "\033[?25h";
        if(i%500==0) printf "\r";
      }
      printf "\033[?25h\033[0m"}'
    ;;
  longline)
    head -c "$n" /dev/zero | tr '\0' 'a'
    printf '\n'
    ;;
esac
