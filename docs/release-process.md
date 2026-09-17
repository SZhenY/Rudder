# 发版 / Release

**简体中文** | [English](#english)

使用发布脚本，让 Git tag 指向的提交本身就已经包含匹配的 Cargo 版本号。

```powershell
.\scripts\release.ps1 v0.5.7 -Push
```

脚本会：

- 要求已跟踪文件没有未提交改动
- 更新 `Cargo.toml` 和 `Cargo.lock` 里的 `meatshell` 版本号
- 运行 `cargo check --locked`
- 验证 `meatshell --version` 输出匹配 tag
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
`Cargo.toml`、`Cargo.lock` 和构建出的 `meatshell --version` 都必须是
`0.5.7`，否则 workflow 会在发布前失败。

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
- updates `Cargo.toml` and the `meatshell` entry in `Cargo.lock`
- runs `cargo check --locked`
- verifies that `meatshell --version` matches the tag
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
`Cargo.toml`, `Cargo.lock`, and the built `meatshell --version` output,
otherwise the workflow fails before publishing.

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
