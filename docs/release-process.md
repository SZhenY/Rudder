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
