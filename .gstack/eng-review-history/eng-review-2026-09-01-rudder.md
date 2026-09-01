# Engineering Review — Rudder (main@6940933)

- Reviewer: gstack-product-reviewer (Eng Review / product dimension)
- Date: 2026-09-01
- Scope: /Users/zheny/Rust/Rudder @ main@6940933
- Relation: complements security audit (`.gstack/security-audit-history/audit-2026-09-01-rudder-comprehensive.md`, F-001..F-006)
- Method: static analysis + cross-verification. No cargo toolchain on host; CI/panic config findings are deterministic facts.

## Verdict: Conditional Pass

Architecture and performance engineering are solid, error handling is generally restrained, security posture B (no direct high-severity flaw). Current version is releasable, but the following must be addressed in the next cycle: CI test gate (E-01), panic fallback + bare-unwrap cleanup (E-02), known_hosts atomic write (E-03), SFTP batch concurrency cap (E-04), disconnect/reconnect UX (E-06). Confidence: High.

## Severity Checklist

### 🔴 Critical — none

### 🟠 High

| ID | Location | Issue | Fix |
|----|----------|-------|-----|
| E-01 | `.github/workflows/release.yml:107-121` | No test/clippy/fmt gate in release pipeline; 249 unit tests never run; regressions ship silently | Add quality job: cargo test + clippy -D warnings + fmt --check, fail = block release |
| E-02 | `Cargo.toml:142` (panic="abort"); `src/main.rs:20` (no panic hook); `src/sftp/impls/sftp.rs:615-617,862-864`; `src/app/session_callbacks.rs:1289-1291,1360-1362` (bare `.lock().unwrap()`) | panic=abort means any background-task panic kills the whole GUI with no diagnostics; several bare unwraps violate the codebase's own poisoned-mutex convention (cf. render_gate.rs:20-23) | Install panic hook (log to error.log + dialog before abort); normalize bare unwraps to `unwrap_or_else(|e| e.into_inner())` |

### 🟡 Medium

| ID | Location | Issue | Fix |
|----|----------|-------|-----|
| E-03 | `src/ssh/impls/known_hosts.rs:98-121` (fs::write at :119) | Non-atomic write, no 0600, no lock; concurrent first-connect accepts lose entries; crash mid-write corrupts file → all hosts "Changed" | Reuse config.rs atomic pattern: temp + rename + 0600; serialize writes |
| E-04 | `src/sftp/impls/sftp.rs:620,751,867` | No file-level concurrency cap for batch transfers; N files → N concurrent tasks × ~1MB in-flight each (MAX_INFLIGHT=32×32KB) | Global semaphore (e.g. max 8 concurrent files) + queue UI + aggregate progress |
| E-05 | `src/tunnel/impls/forward.rs:141-207`; `src/app/port_forward.rs:40-101` | No per-listener connection cap; UI validates only port range, bind_addr passthrough unvetted (ties to security F-001) | Warn on non-loopback bind (esp. 0.0.0.0 dynamic SOCKS5); per-listener connection cap; source/dest audit log |
| E-06 | `src/ssh/impls/ssh.rs:2763-2768` | No auto-reconnect / session recovery; manual rebuild required after network blip | Show disconnect reason + one-click reconnect; optional auto-reconnect with backoff |
| E-07 | `src/terminal/impls/local.rs:120`; `src/terminal/impls/encoding.rs:4-5` | Local shell forced UTF-8 (`from_utf8_lossy`); GBK/CP936 locale local terminals show mojibake while SSH has per-session encoding | Reuse TerminalEncoding for local shells with per-session/locale encoding option |
| E-08 | `src/app/session_runtime.rs:65` | Unbounded SFTP event channel (acknowledged in comment) | Bounded channel + drop policy (progress events droppable; list events must be ordered+delivered) |

### 🟢 Low / Improvement

- E-09 `tests/` has no integration tests (only fixtures + .txt); SSH/SFTP/tunnel/Zmodem lack end-to-end coverage.
- E-10 CI lacks fmt/clippy (fold into E-01).
- E-11 `src/app/updater.rs:21-28` update button only opens releases page; no in-app download / SHA / signature verification (supply-chain 10-star direction).
- E-12 `src/app/updater.rs:47-53` network failure fully silent; user cannot tell check was skipped.
- E-13 `Cargo.toml:143` overflow-checks=false; integer wraparound for remote-derived sizes — recommend enabling or auditing (defense in depth; no direct exploit found by security audit).

## Verified Strengths

- Mature rendering pipeline: render_gate + event-pump batching + DRAIN_CAP=2048 + OUTPUT_MERGE_BYTE_CAP handles `tail -f` floods (#171/#209) without UI freeze.
- Config atomic save: temp+rename+0600 (`config.rs:2246-2260`) + backup sync (`:2265-2312`).
- SFTP per-file pipeline MAX_INFLIGHT=32≈1MB hides RTT (`sftp.rs:1892-1893`); per-file cancel registration.
- Port-forward default binds loopback 127.0.0.1 (`forward.rs:27-32`).
- Restrained error handling: most lock() use `unwrap_or_else(|e| e.into_inner())`; no bare panic surface except E-02 spots.
- Encoder keeps state across SSH packets (multibyte split).
- OSC7 cwd tracking + 500ms debounce + duplicate suppression (`session_runtime.rs:161-174`).
- Terminal output ingested off the UI thread.

## Top 5 Actions

1. (E-01) Add quality gate to release.yml: cargo test + clippy -D warnings + fmt --check.
2. (E-02) Install panic hook; normalize bare `.lock().unwrap()` at sftp.rs:615/862 and session_callbacks.rs:1289/1360.
3. (E-03) Make known_hosts remember() atomic (temp+rename+0600).
4. (E-04 + E-05) SFTP global concurrency semaphore (max ~8) + queue; forward per-listener cap + non-loopback bind warning (aligns with F-001).
5. (E-06) Disconnect reason + one-click reconnect; later auto-reconnect + integration tests (local sshd or mock russh).

## Merge Notes for Team Lead

- E-05 ↔ F-001 (SOCKS5 exposure) same root cause.
- E-02 ↔ F-001 resource-exhaustion portion overlap.
- Merge into unified fix list with security officer's F-001..F-006.
