# 发版 / Release

**简体中文** | [English](#english)

使用发布脚本，让 Git tag 指向的提交本身就已经包含匹配的 Cargo 版本号。

```powershell
.\scripts\release.ps1 v0.5.7 -Push
```

脚本会：

- 要求已跟踪文件没有未提交改动
- 更新 `Cargo.toml` 和 `Cargo.lock` 里的 `rudder` 版本号
- 运行 `cargo check --locked`
- 验证 `rudder --version` 输出匹配 tag
- 提交版本号变更
- 创建 annotated tag
- 传入 `-Push` 时推送当前分支和 tag

如果想先在本地创建提交和 tag，不立即推送：

```powershell
.\scripts\release.ps1 v0.5.7
git push origin HEAD
git push origin v0.5.7
```

Release workflow 也会检查推送上来的 tag。比如 tag 名是 `v0.5.7` 时，
`Cargo.toml`、`Cargo.lock` 和构建出的 `rudder --version` 都必须是
`0.5.7`，否则 workflow 会在发布前失败。

### CHANGELOG 与发布说明

`CHANGELOG.md` 在发版前写好 `## [Unreleased]`，经检查确认后由 `release.sh` 提升为
版本小节。脚本不会从 git 历史自动生成内容；`preflight.sh` 会全量列出本次提交供逐条核对。

GitHub Release 正文由 `scripts/release-notes.sh` 生成：先是该版本的手写段落，末尾附
上一版 tag 到当前 tag 的**全部提交标题**。CI 的 checkout 已设 `fetch-depth: 0` 与
`fetch-tags: true` 以取得完整历史；`size-report.sh` 只在末尾追加包体积段。

CHANGELOG 与 commit message 只写版本之间可观察的差异：

- 开发过程中发现、且未推送/未发版的 bug 修复不写，并入功能提交
- GitHub issue 已报告的 bug 与发版后修复的 bug 必须写进 CHANGELOG

### 包体积记录

tag 推送后，workflow 末尾的 `包体积` 作业会把各平台体积写进 Release 正文：

- **可执行文件**：未压缩的二进制大小（各构建作业上传的 `size-*.json`，压缩包里看不到）
- **安装包**：zip / tar.gz 的实际大小，AppImage 与 MSI 单列在表下方
- **与上一版相比**：与上一份 Release 同名资产的差值（按 `windows-` / `linux-` / `macos-`
  后缀对齐，版本号不同也能配上）

正式版（tag 不含 `-alpha` / `-beta` / `-rc`）还会检查两个 Windows MSI：缺任一个就在这段
末尾写一行 `⚠ 正式版缺少 Windows MSI`，脚本以非 0 退出，`包体积` 作业标红。安装版用户的
自更新只认 msi（见 `src/app/self_updater.rs` 的 `install_kind`），缺了等于他们收不到更新。

这段由 `scripts/size-report.sh` 生成，正文里带 `<!-- size-report:start -->` 标记，
重跑会整段替换而不是叠加。只想预览不写 Release：

```bash
bash scripts/size-report.sh v0.8.0-beta3 --dry-run
```

### 「挂产物」步骤失败时（`Attach to GitHub Release`）

如果构建、打包、`Upload workflow artifact` 都成功，只有最后一步挂产物报
`Headers Timeout Error` 或 `Error saving asset`，那是 GitHub 侧的瞬时故障，
**不是编译失败**，重跑即可：

```bash
gh run view <run-id>              # 先确认失败的是哪个作业的哪一步
gh run rerun <run-id> --failed    # 只重跑失败的作业，其余作业不动（约 15 分钟）
```

产物带 `overwrite_files: true`，重复上传同名文件会覆盖，所以重跑是安全的。
若只有个别产物没挂上，也可以从该次运行的 workflow artifact 下载后手工补：

```bash
gh release upload v0.7.8 <文件> --clobber
```

<a name="english"></a>

## English

Use the release helper so the tag points at a commit whose Cargo package version
matches the tag.

```powershell
.\scripts\release.ps1 v0.5.7 -Push
```

The script:

- requires no uncommitted tracked-file changes
- updates `Cargo.toml` and the `rudder` entry in `Cargo.lock`
- runs `cargo check --locked`
- verifies that `rudder --version` matches the tag
- commits the version bump
- creates an annotated tag
- pushes the current branch and tag when `-Push` is passed

To prepare the commit and tag without pushing:

```powershell
.\scripts\release.ps1 v0.5.7
git push origin HEAD
git push origin v0.5.7
```

The release workflow also checks pushed tags. A tag named `v0.5.7` must match
`Cargo.toml`, `Cargo.lock`, and the built `rudder --version` output,
otherwise the workflow fails before publishing.

### CHANGELOG and release notes

`CHANGELOG.md` is written into `## [Unreleased]` before a release and promoted to a
version section by `release.sh`. Nothing is generated from git history; `preflight.sh`
lists every commit in the range for a line-by-line check.

`scripts/release-notes.sh` builds the GitHub Release body: the hand-written section,
followed by the full list of commit subjects since the previous tag. CI checkouts use
`fetch-depth: 0` and `fetch-tags: true`; `size-report.sh` only appends the size table.

Only user-observable differences between released versions belong in the CHANGELOG
and commit messages: bugs found and fixed during development before any push or
release are folded into the feature commit; bugs reported in a GitHub issue and bugs
fixed after a release must be written into the CHANGELOG.

### Package sizes in the release notes

After a tag is pushed, the `包体积` (`size-report`) job appends a size table to
the release body:

- **executable**: uncompressed binary size (from the per-job `size-*.json`;
  the archives don't reveal it)
- **package**: actual zip / tar.gz size; AppImage and MSI are listed below the table
- **vs. previous release**: delta against the same asset of the previous release
  (matched by the `windows-` / `linux-` / `macos-` suffix, so different version
  numbers still line up)

For stable releases (tags without `-alpha` / `-beta` / `-rc`) the script also
requires both Windows MSIs: when one is missing it appends a
`⚠ 正式版缺少 Windows MSI` line and exits non-zero, which marks the `size-report`
job red. MSI installs only self-update from an `.msi` (see `install_kind` in
`src/app/self_updater.rs`), so a missing MSI means those users get no update.

The table is produced by `scripts/size-report.sh` and wrapped in
`<!-- size-report:start -->` markers, so reruns replace it instead of stacking.
Preview without touching the release:

```bash
bash scripts/size-report.sh v0.8.0-beta3 --dry-run
```

### When the attach step fails (`Attach to GitHub Release`)

If the build, packaging and `Upload workflow artifact` all succeeded and only
the final attach step failed with `Headers Timeout Error` or `Error saving
asset`, that is a transient GitHub-side failure — **not a compile error**.
Just rerun the failed jobs:

```bash
gh run view <run-id>              # confirm which job/step failed first
gh run rerun <run-id> --failed    # retries only the failed jobs (~15 min)
```

Attaching uses `overwrite_files: true`, so uploading the same name again
overwrites it and rerunning is safe. For a single missing asset you can also
grab the workflow artifact from that run and upload it by hand:

```bash
gh release upload v0.7.8 <file> --clobber
```
