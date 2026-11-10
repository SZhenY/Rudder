use super::*;

/// 按**更新通道**挑出最新的一个 release（返回它的 JSON，调用方再读 `tag_name` / `assets`）。
///
/// * `stable`：走 `/releases/latest` —— GitHub 保证它不含预发布，响应也最小；
/// * `beta` / `all`：拉一页 `/releases` 自己挑 —— GitHub **没有**"最新测试版"这种端点。
///   `beta` 只认测试版（`-alphaN` / `-betaN` / `-rcN`，即 `is_test_build`），
///   `all`（全通道最新版）认版本最高的那个，不挑类型 —— 于是它同时包含测试版与正式版。
///   排序按 `parse_version` 的约定：**同号的测试版比正式版新**（`0.7.9-beta1 > 0.7.9`），
///   所以正式版之后发出去的测试版能被提示到（见下面的 `channel_tests`）。
///
/// 预发布与否按 **tag 名**判断，而不是 API 的 `prerelease` 字段：tag 是权威
/// （`-betaN` 的语义写在我们自己的版本解析里），API 字段只是发布时的标记。
pub(crate) fn fetch_channel_release(channel: &str) -> Result<Option<serde_json::Value>> {
    let url = if channel == "stable" {
        format!("https://api.github.com/repos/{REPO_OWNER}/{REPO_NAME}/releases/latest")
    } else {
        format!("https://api.github.com/repos/{REPO_OWNER}/{REPO_NAME}/releases?per_page=30")
    };
    let client = crate::app::update_net::current()
        .client()
        .map_err(|e| anyhow!("{e}"))?;
    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .map_err(|e| anyhow!("{}", crate::app::update_net::describe(&e)))?
        .error_for_status()
        .map_err(|e| anyhow!("{}", crate::app::update_net::describe(&e)))?;
    let body = read_body_capped(resp, MAX_RELEASES_BODY, "releases")?;
    let json: serde_json::Value = serde_json::from_str(&body).context("parse releases response")?;
    if channel == "stable" {
        return Ok(Some(json));
    }

    let list = json.as_array().cloned().unwrap_or_default();
    Ok(pick_channel_release(&list, channel))
}

/// 从一页 `/releases` 的结果里挑出该通道要提示的那一个（`stable` 不走这里，它直接读
/// `/releases/latest`）。
///
/// 跳过草稿、以及 tag 认不出格式的项；`beta` 通道只要测试版（`is_test_build`），
/// `all` 什么都认 —— 取版本最高的那个。
///
/// 抽成纯函数是为了**能测**：这段挑选逻辑原先和网络请求缠在一起，唯一"验证"它的是线上的
/// 真实 release，而 `-betaN` 的排序约定（同号比正式版新）恰好是错的 —— 没有任何测试盯着。
pub(crate) fn pick_channel_release(
    list: &[serde_json::Value],
    channel: &str,
) -> Option<serde_json::Value> {
    let mut best: Option<(&serde_json::Value, crate::app::Version)> = None;
    for release in list {
        if release["draft"].as_bool().unwrap_or(false) {
            continue;
        }
        let Some(v) = crate::app::parse_version(release["tag_name"].as_str().unwrap_or_default())
        else {
            continue;
        };
        if channel == "beta" && !crate::app::is_test_build(&v) {
            continue;
        }
        if best.as_ref().is_none_or(|(_, best_v)| v > *best_v) {
            best = Some((release, v));
        }
    }
    best.map(|(release, _)| release.clone())
}

/// 查询 GitHub Releases，返回比 `current` 新的最新版本（无则 None）。
///
/// `channel` 决定查哪一类 release（见 [`fetch_channel_release`]）。
/// 静默策略沿用既有检查逻辑：网络/解析失败返回 Err，但调用方只记录日志。
pub(crate) fn latest_update(
    current: crate::app::Version,
    channel: &str,
) -> Result<Option<UpdateCandidate>> {
    let Some(json) = fetch_channel_release(channel)? else {
        return Ok(None);
    };
    let tag = json["tag_name"].as_str().unwrap_or_default().to_string();
    let Some(latest) = crate::app::parse_version(&tag) else {
        return Ok(None);
    };
    if latest == current {
        return Ok(None);
    }
    let is_switch = latest < current;
    if is_switch && !is_channel_switch(channel, current, latest) {
        return Ok(None);
    }

    // 挑资产：先按扩展名过滤，再按平台关键字（按优先级）匹配。
    // MSI 安装要 msi（交给系统安装程序去覆盖 Program Files 里的文件），便携版要
    // zip / tar.gz（替换 exe）。
    let msi = install_kind() == InstallKind::Msi;
    // 安装版不做降级：`wix/main.wxs` 的 MajorUpgrade 明确禁止降级
    // （DowngradeErrorMessage），把旧包交给 msiexec 只会弹一句"已安装更新的版本"
    // 然后退出。宁可判"无更新"，也不要让用户看到安装程序报错。
    if msi && is_switch {
        tracing::info!("self-update: MSI 安装不支持降级到 {tag}，已跳过");
        return Ok(None);
    }
    let ext = asset_extension_for(std::env::consts::OS, msi);
    let empty = Vec::new();
    let assets = json["assets"].as_array().unwrap_or(&empty);
    let candidates: Vec<(String, String)> = assets
        .iter()
        .filter_map(|a| {
            let name = a["name"].as_str()?;
            let url = a["browser_download_url"].as_str()?;
            if !name.ends_with(ext) {
                return None;
            }
            Some((name.to_string(), url.to_string()))
        })
        .collect();

    let notes = notes_for(&tag, json["body"].as_str().unwrap_or_default());
    for kw in asset_keywords_for(
        std::env::consts::OS,
        std::env::consts::ARCH,
        current_glibc(),
        msi,
    ) {
        if let Some((name, url)) = candidates.iter().find(|(n, _)| n.contains(kw)) {
            return Ok(Some(UpdateCandidate {
                version: tag.trim_start_matches('v').to_string(),
                asset_name: name.clone(),
                download_url: url.clone(),
                notes: notes.clone(),
                is_switch,
                is_msi: msi,
            }));
        }
    }
    // 安装版遇到不带 msi 的版本（预发布版按设计不产 msi）。这里只能判"无更新"，
    // 但必须留一条日志：否则用户切到测试版通道后看到"已是最新版本"，无从判断
    // 是自己已最新还是这个版本根本没提供安装包。
    if msi {
        tracing::info!("self-update: {tag} 未提供 MSI 安装包，安装版无法更新到该版本");
    }
    Ok(None)
}
