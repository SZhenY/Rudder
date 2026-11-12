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

/// 目标平台：资产匹配要看的四个维度。
pub(crate) struct AssetTarget<'a> {
    pub os: &'a str,
    pub arch: &'a str,
    pub glibc: Option<(u32, u32)>,
    pub msi: bool,
}

/// 一次查询的结局。调用方必须能区分「真的最新」与「有新版但本平台没包」，
/// 否则手动检查只会显示一句会骗人的「已是最新版本」。
#[derive(Debug, Clone)]
pub(crate) enum UpdateLookup {
    UpToDate,
    NoAssetForPlatform { version: String },
    NoMsiForPlatform { version: String },
    Found(UpdateCandidate),
}

impl UpdateLookup {
    #[cfg(test)]
    pub(crate) fn candidate(self) -> Option<UpdateCandidate> {
        match self {
            UpdateLookup::Found(candidate) => Some(candidate),
            _ => None,
        }
    }
}

/// 把一份 release JSON 解读成"本平台的结局"。纯函数：不联网、不取发布说明。
pub(crate) fn lookup_release(
    json: &serde_json::Value,
    current: crate::app::Version,
    channel: &str,
    target: AssetTarget<'_>,
) -> UpdateLookup {
    let AssetTarget { os, arch, glibc, msi } = target;
    let tag = json["tag_name"].as_str().unwrap_or_default().to_string();
    let version = tag.trim_start_matches('v').to_string();
    let Some(latest) = crate::app::parse_version(&tag) else {
        return UpdateLookup::UpToDate;
    };
    if latest == current {
        return UpdateLookup::UpToDate;
    }
    let is_switch = latest < current;
    if is_switch && !is_channel_switch(channel, current, latest) {
        return UpdateLookup::UpToDate;
    }

    // 挑资产：先按扩展名过滤，再按平台关键字（按优先级）匹配。
    // MSI 安装要 msi（交给系统安装程序去覆盖 Program Files 里的文件），便携版要
    // zip / tar.gz（替换 exe）。
    // 安装版不做降级：`wix/main.wxs` 的 MajorUpgrade 明确禁止降级
    // （DowngradeErrorMessage），把旧包交给 msiexec 只会弹一句"已安装更新的版本"
    // 然后退出。宁可判"无更新"，也不要让用户看到安装程序报错。
    if msi && is_switch {
        tracing::info!("self-update: MSI 安装不支持降级到 {tag}，已跳过");
        return UpdateLookup::UpToDate;
    }
    let ext = asset_extension_for(os, msi);
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

    if candidates.is_empty() {
        // 发版后 CI 还在上传时就是这条：release 已存在，本平台的包还没传上来。
        tracing::info!("self-update: {tag} 还没有任何 {ext} 资产（可能仍在构建 / 上传）");
        return if msi {
            UpdateLookup::NoMsiForPlatform { version }
        } else {
            UpdateLookup::NoAssetForPlatform { version }
        };
    }
    for kw in asset_keywords_for(os, arch, glibc, msi) {
        if let Some((name, url)) = candidates.iter().find(|(n, _)| n.contains(kw)) {
            return UpdateLookup::Found(UpdateCandidate {
                version,
                asset_name: name.clone(),
                download_url: url.clone(),
                notes: String::new(),
                is_switch,
                is_msi: msi,
            });
        }
    }
    let keywords = asset_keywords_for(os, arch, glibc, msi);
    tracing::info!(
        "self-update: {tag} 的资产里没有匹配 {} 的本平台包",
        keywords.first().copied().unwrap_or("")
    );
    // 安装版遇到不带 msi 的版本（预发布版按设计不产 msi）。这里只能判"无更新"，
    // 但必须留一条日志：否则用户切到测试版通道后看到"已是最新版本"，无从判断
    // 是自己已最新还是这个版本根本没提供安装包。
    if msi {
        tracing::info!("self-update: {tag} 未提供 MSI 安装包，安装版无法更新到该版本");
    }
    UpdateLookup::NoAssetForPlatform { version }
}

/// 兼容壳：现有测试按 `Option` 断言。
#[cfg(test)]
pub(crate) fn candidate_from_release(
    json: &serde_json::Value,
    current: crate::app::Version,
    channel: &str,
    target: AssetTarget<'_>,
) -> Option<UpdateCandidate> {
    lookup_release(json, current, channel, target).candidate()
}

/// 查询 GitHub Releases，返回本次查询的结局（见 [`UpdateLookup`]）。
///
/// `channel` 决定查哪一类 release（见 [`fetch_channel_release`]）。
/// 静默策略沿用既有检查逻辑：网络/解析失败返回 Err，但调用方只记录日志。
pub(crate) fn latest_update(
    current: crate::app::Version,
    channel: &str,
) -> Result<UpdateLookup> {
    let Some(json) = fetch_channel_release(channel)? else {
        return Ok(UpdateLookup::UpToDate);
    };
    let msi = install_kind() == InstallKind::Msi;
    let mut lookup = lookup_release(
        &json,
        current,
        channel,
        AssetTarget {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            glibc: current_glibc(),
            msi,
        },
    );
    // 只有确实挑到资产才去拉 CHANGELOG：发版瞬间资产未就绪时省掉一次必然落空的请求。
    if let UpdateLookup::Found(candidate) = &mut lookup {
        candidate.notes = notes_for(
            &format!("v{}", candidate.version),
            json["body"].as_str().unwrap_or_default(),
        );
    }
    Ok(lookup)
}
