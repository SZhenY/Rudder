# Security Posture Report — Rudder (main@6940933)

## Meta
- Audit mode: Comprehensive (focused: 8 areas — credentials / host-key / OSC52 / path traversal / port-forward / command injection / unsafe / dependency CVE)
- Date: 2026-09-01
- Scope: /Users/zheny/Rust/Rudder @ main@6940933
- Method: manual static analysis + active verification (no cargo toolchain available on host; CVE status verified via RustSec/advisory databases)

## Executive Summary
Rudder 的整体安全姿态良好：凭据加密、主机密钥 TOFU（fail-closed）、路径净化、命令注入防护均实现扎实。
发现的实质问题集中在「远程侧向本地的信任面」——SOCKS5 动态转发无认证（可被局域网利用为开放代理）、OSC52 剪贴板投毒（设计功能）、WebDAV 任意证书开关（MITM）。
依赖面唯一的已知 advisory（russh 0.49.2）均经核实不影响 client-only 部署，但建议纳入升级规划。

## Findings

### [F-001] SOCKS5 动态端口转发无认证，可被局域网利用为开放代理
- **Category**: OWASP A01 (Broken Access Control) / STRIDE: Spoofing, Information Disclosure
- **Severity**: High
- **Confidence**: 9
- **Location**: `src/tunnel/impls/forward.rs:141-207` (socks5_serve), `src/tunnel/impls/forward.rs:107-138` (spawn_dynamic), `src/app/port_forward.rs:40-101`
- **Description**: 动态转发实现的是无认证 SOCKS5（NO AUTH 握手，CONNECT 模式），可代连任意主机。`bind_addr` 为空时默认 `127.0.0.1`，但 UI 校验未限制用户填写 `0.0.0.0`。
- **Exploit Scenario**: 用户在公网/共享 WiFi 环境将动态转发绑定 `0.0.0.0:1080` → 局域网任意设备可把该机器当作匿名 SOCKS5 代理 → 用于隐蔽出网、绕过内网访问控制、消耗带宽。
- **Reproduction**: `forward.rs` 中 socks5_serve 接受无认证握手即进入 CONNECT 转发；`port_forward.rs` 仅校验端口号范围，bind_addr 字符串透传。
- **Remediation**: ① 默认保持 127.0.0.1（已实现）；② bind_addr 选择 UI 增加警示；③ 为动态转发增加可选用户名/密码认证（SOCKS5 user/pass）；④ 记录每个连接的来源 IP 与目标以便审计。
- **Priority**: P1

### [F-002] OSC52 剪贴板投毒（远程可写本地剪贴板）
- **Category**: OWASP A05 (Security Misconfiguration) / STRIDE: Elevation of Privilege (社会工程链)
- **Severity**: Medium
- **Confidence**: 9
- **Location**: `src/terminal/impls/vt_adapter.rs:16` (OSC52_ENABLED 默认 true), `src/terminal/impls/vt_adapter.rs:213-240` (osc52_extract), `src/terminal/impls/vt_adapter.rs:31-52` (osc52_writer bounded channel)
- **Description**: 终端字节流中提取 OSC 52 序列并写入系统剪贴板。恶意/被攻陷的远程服务器可在用户不知情时覆写剪贴板，诱导用户粘贴恶意命令或地址。已有防洪泛设计（单后台线程 + 16 容量 bounded channel），但功能默认开启。
- **Exploit Scenario**: 用户 ssh 到被攻陷主机 → 该主机输出 `ESC]52;c;<base64(payload)>` → 用户剪贴板被替换 → 用户在浏览器/终端粘贴时误执行恶意内容。
- **Reproduction**: 任意远程 shell 输出构造 OSC 52 序列即可触发（代码路径确认）。
- **Remediation**: ① 默认改为「询问后允许」或提供每会话开关；② 对超过阈值（如 4 KiB）的剪贴板写入二次确认；③ 文档提示用户开启提示模式。
- **Priority**: P2

### [F-003] WebDAV `accept_invalid_certs` 开启后接受任意 TLS 证书（MITM）
- **Category**: OWASP A02 (Cryptographic Failures) / STRIDE: Spoofing, Tampering
- **Severity**: Medium
- **Confidence**: 9
- **Location**: `src/webdav/impls/certificate_verifier.rs:3-14` (verify_server_cert 无条件 `ServerCertVerified::assertion()`), `src/app/webdav.rs:92-106` (webdav_agent)
- **Description**: `webdav_accept_invalid_certs` 开启后使用无条件信任证书的 verifier，WebDAV 流量可被中间人篡改/窃听（含可能存在的凭据）。默认关闭，属用户显式选择，但 UI 上未突出风险。
- **Exploit Scenario**: 用户为调试开启该开关并连接公网 WebDAV → 攻击者 MITM → 读取/篡改传输内容。
- **Reproduction**: 代码路径确认：`verify_server_cert` 恒返回 assertion。
- **Remediation**: ① 开关处展示持久化警告；② 仅对自签名证书场景建议使用，建议支持「pin 该证书指纹」替代全局关闭校验。
- **Priority**: P2

### [F-004] russh 0.49.2 存在 3 个未修补 advisory（均不影响 client-only 部署，Watch）
- **Category**: OWASP A06 (Vulnerable and Outdated Components)
- **Severity**: Info / Watch
- **Confidence**: 9
- **Location**: `Cargo.lock:5382-5383` (russh 0.49.2), `audit.toml` (ignore RUSTSEC-2026-0154)
- **Description**:
  - RUSTSEC-2026-0154 — SSH-agent frame 解析无界 32-bit 分配（DoS）。audit.toml 已核实声明不可达：Rudder 完全不用 ssh-agent。
  - CVE-2026-73430 (GHSA-5xvq-cp9x-6p6r, <0.62.4) — **server 侧** pre-auth panic（all-zero Curve25519 Q_C）。Rudder 是 client，非漏洞方。
  - CVE-2026-73489 (GHSA-cqjc-rmpq-xprq, <0.62.4) — **server 侧** post-auth panic（pty-req >130 terminal-mode）。同上。
- **Reproduction**: 三个 advisory 均核实为 server 角色或 agent 路径，Rudder 均不可达。
- **Remediation**: 规划升级 russh ≥ 0.62.4（当前阻塞点：升级会引入 pre-release 密码学 crate，见 audit.toml 论证）。建议在 crypto 依赖出 -rc/-pre 后执行 PR #151 形态的 API 迁移；在此之前保持 audit.toml 说明并定期复查。
- **Priority**: P3 (watch)

### [F-005] `sudo -S` root 密码经 SSH 通道发送给远程主机（设计固有残余风险）
- **Category**: STRIDE: Information Disclosure
- **Severity**: Low
- **Confidence**: 5
- **Location**: `src/ssh/impls/ssh.rs:1136-1235` (kill_remote_process), `src/ssh/impls/ssh.rs:1340-1349` (process_kill_command)
- **Description**: 已确认：密码不嵌入命令行（`sudo -S -p 'Password:' -- kill -TERM {pid}` 中无密码）；密码经 `channel.data()`（SSH 加密通道 stdin）在检测到 sudo 提示符后发送；PTY 层 ECHO=0；日志经 `process_control_log_text` 脱敏；发送后 `zeroize`。残余风险是：用户主动向远程主机提供 root 密码，恶意/被攻陷主机可捕获该密码（功能固有）。`pid` 为 `u32`，无命令注入面。
- **Exploit Scenario**: 用户对不信任的远程主机使用「root 密码结束进程」→ 该主机窃取密码。
- **Reproduction**: 代码路径确认（密码经 stdin 而非 argv，无注入面）。
- **Remediation**: UI 提示「root 密码将发送至远程主机，请仅对可信主机使用」。
- **Priority**: P3

### [F-006] 兼容 KEX/加密算法包含旧算法（DH_G1_SHA1、3DES-CBC 等）
- **Category**: OWASP A02 (Cryptographic Failures) / STRIDE: Tampering
- **Severity**: Low
- **Confidence**: 6
- **Location**: `src/ssh/impls/ssh.rs:1694-1723` (COMPAT_KEX / COMPAT_CIPHER)
- **Description**: 算法列表 curve25519/chacha20-poly1305 优先，但为兼容旧服务器保留了 DH_G1_SHA1、3DES-CBC 等。若服务器只支持旧算法，会话将协商到弱算法；MITM 防护仍由 host key TOFU（fail-closed）兜底。
- **Reproduction**: 与仅支持旧算法的服务器连接时触发（需确认服务器端配置）。
- **Remediation**: 评估是否移除 DH_G1_SHA1 与 3DES-CBC；若需保留，至少在 UI 连接详情中展示协商算法供用户识别。
- **Priority**: P3

## Verified Non-Findings（核验为良好设计，无漏洞）

| 领域 | 结论 | 证据 |
|---|---|---|
| 凭据加密 | ✅ 良好 | ChaCha20-Poly1305 + 12B 随机 nonce + base64url；`ConfigStore::load_or_create_key` 0600；`save` 临时文件+原子重命名 0600；`Secret` zeroize+Debug 脱敏；`sync_backup` 0600 |
| 主机密钥 TOFU | ✅ 良好（fail-closed） | `known_hosts.rs:76-94` Unknown/Match/Changed；`ssh.rs:3377-3411` UI 关闭即拒绝；SFTP/测试路径同流程 |
| SFTP/Zmodem 路径穿越 | ✅ 良好 | `sftp.rs:1580-1630` sanitize_filename 剔除分隔符+shell 特殊字符+Windows 保留名；`zmodem.rs:482-496` rsplit 取 basename；`sftp.rs:1490-1492` sh_quote 正确转义；`open_with_os` 用 ShellExecuteW 绕过 cmd /C |
| 粘贴转义 | ✅ 良好 | `input.rs:69-83` bracketed paste 过滤 ESC/Ctrl+C |
| 命令注入 | ✅ 良好 | 本地终端 CommandBuilder+arg() 无 shell；SFTP tar 命令参数全经 sh_quote；process_kill pid 为 u32；密码走 stdin |
| unsafe | ✅ 无风险 | 仅 Windows FFI（GetKeyState/SystemParametersInfoW/ShellExecuteW/GetCursorPos），参数固定 |
| SSH config Include | ✅ 良好 | `ssh_config.rs:21` MAX_INCLUDE_DEPTH=16 + visited 循环检测 |
| PPK 解析 | ✅ 良好 | `ppk.rs:103-111` Argon2 参数上限拒绝不安全配置 |
| 依赖 CVE | ✅ regex/serde_json/tokio/rustls/ring 均不受影响 | regex 1.13.1 > 1.5.5（CVE-2022-24713 已修复）；serde_json 1.0.151 > 1.0.121；tokio 1.53.1 无适用 advisory |

## Security Posture Score
- Critical: 0
- High: 1
- Medium: 2
- Low: 2
- Info/Watch: 1
- Overall: **B**（无关键/高危直通漏洞；主要风险为条件触发的信任面问题）

## STRIDE 威胁建模表

| 类别 | 威胁场景 | 状态 | 缓解/证据 |
|---|---|---|---|
| Spoofing | 主机冒充 / MITM | ✅ 已缓解 | TOFU host key 校验 fail-closed（known_hosts.rs + ssh.rs:3377）；WebDAV 例外为用户开关（F-003） |
| Tampering | 传输篡改 | ✅ 已缓解 | SSH AEAD（chacha20-poly1305）+ rustls TLS（更新/WebDAV 默认校验）；旧算法兜底存在（F-006） |
| Repudiation | 操作抵赖 | ⚠️ 部分 | error.log 有诊断记录；但无结构化安全审计事件（认证失败、host key 变更、转发连接） |
| Information Disclosure | 凭据泄露 | ✅ 已缓解 | 凭据 ChaCha20-Poly1305 加密 + 0600 + zeroize；SOCKS5 开放代理为泄露通道风险（F-001）；sudo 密码固有风险（F-005） |
| Denial of Service | 资源耗尽 | ⚠️ 部分 | OSC52 bounded queue (16) 防洪泛；SOCKS5 无连接数/带宽限制（F-001）；依赖面无可用 DoS |
| Elevation of Privilege | 越权/诱导 | ⚠️ 部分 | OSC52 剪贴板投毒（F-002）可诱导用户在更高权限上下文执行恶意内容；无 RCE 直通 |

## OWASP Top 10 映射
- **A01 Broken Access Control** → F-001（SOCKS5 无认证开放代理）— High
- **A02 Cryptographic Failures** → F-003（WebDAV 任意证书）；F-006（旧算法兜底）— Medium/Low；EXPORT_KEY 硬编码为混淆密钥（config.rs:1056，注释已明示非安全用途）— Info
- **A03 Injection** → 无发现（sh_quote / sanitize_filename / CommandBuilder / u32 pid / 无 shell 解析）
- **A04 Insecure Design** → F-002（OSC52 剪贴板写面设计取舍）— Medium
- **A05 Security Misconfiguration** → F-003（调试开关）；COMPAT 旧算法默认在列 — Medium/Low
- **A06 Vulnerable/Outdated Components** → F-004（russh 0.49.2 watch）— Info
- **A07 Identification/Auth Failures** → 无发现（密码强哈希/Argon2 参数上限/TOFU）
- **A08 Software/Data Integrity** → 无发现（更新路径 HTTPS+rustls；原子写）
- **A09 Logging/Monitoring** → 无安全事件审计日志（建议增强）— Info
- **A10 SSRF** → 无发现（无用户可控 URL 抓取面；更新 URL 固定）

## Top 5 修复建议
1. **（P1）SOCKS5 动态转发增加认证或至少 UI 强警示 0.0.0.0 绑定风险** — 消除局域网开放代理面
2. **（P2）OSC52 默认改为询问/提示模式** — 消除远程剪贴板投毒诱导链
3. **（P2）WebDAV 任意证书开关处持久化风险提示，推荐证书指纹 pin 替代全局关闭** — 降低 MITM 面
4. **（P3）规划 russh ≥ 0.62.4 升级路线**（待 crypto 依赖稳定），期间保持 audit.toml 论证并每季度复查
5. **（P3）建立安全审计事件日志**（host key 变更、认证失败、端口转发连接）与 UI 提示 root 密码仅用于可信主机
