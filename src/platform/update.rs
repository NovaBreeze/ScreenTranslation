use anyhow::{Context, Result, anyhow, bail};
use reqwest::Client;
use serde::Deserialize;
use std::{path::Path, process::Command, time::Duration};

const RELEASE_API: &str =
    "https://api.github.com/repos/NovaBreeze/ScreenTranslation/releases/latest";

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub version: String,
    pub download_url: String,
    pub release_page: String,
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

pub async fn check(current_version: &str) -> Result<Option<UpdateInfo>> {
    let release = Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?
        .get(RELEASE_API)
        .header("User-Agent", format!("ScreenTranslator/{current_version}"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("检查 GitHub Releases 失败")?
        .error_for_status()
        .context("GitHub Releases 返回错误")?
        .json::<GitHubRelease>()
        .await
        .context("解析 GitHub Release 失败")?;

    let version = release.tag_name.trim_start_matches(['v', 'V']).to_owned();
    if !is_newer(&version, current_version) {
        return Ok(None);
    }
    let asset = release
        .assets
        .iter()
        .find(|asset| {
            let name = asset.name.to_ascii_lowercase();
            name.ends_with(".zip") && name.contains("win64")
        })
        .or_else(|| {
            release.assets.iter().find(|asset| {
                let name = asset.name.to_ascii_lowercase();
                name.ends_with(".zip") && name.contains("windows")
            })
        })
        .ok_or_else(|| anyhow!("Release {version} 没有 Windows x64 ZIP 资源"))?;
    Ok(Some(UpdateInfo {
        version,
        download_url: asset.browser_download_url.clone(),
        release_page: release.html_url,
    }))
}

pub async fn download_and_schedule(info: &UpdateInfo) -> Result<()> {
    let current_exe = std::env::current_exe().context("无法定位当前程序")?;
    let lower = current_exe.to_string_lossy().to_ascii_lowercase();
    if lower.contains(r"\target\debug\") || lower.contains(r"\target\release\") {
        bail!("开发构建不会自动覆盖；请从 {} 手动下载", info.release_page);
    }

    let update_root = std::env::temp_dir().join(format!(
        "ScreenTranslator-update-{}-{}",
        info.version,
        std::process::id()
    ));
    std::fs::create_dir_all(&update_root).context("创建更新临时目录失败")?;
    let archive = update_root.join("update.zip");
    let bytes = Client::builder()
        .timeout(Duration::from_secs(180))
        .build()?
        .get(&info.download_url)
        .header(
            "User-Agent",
            format!("ScreenTranslator/{}", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .context("下载更新失败")?
        .error_for_status()
        .context("下载更新返回错误")?
        .bytes()
        .await
        .context("读取更新包失败")?;
    std::fs::write(&archive, bytes).context("保存更新包失败")?;

    let script = update_root.join("apply-update.ps1");
    std::fs::write(
        &script,
        update_script(
            std::process::id(),
            &archive,
            current_exe.parent().context("无法定位程序安装目录")?,
            &update_root,
        ),
    )
    .context("写入更新脚本失败")?;
    Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-File",
        ])
        .arg(script)
        .spawn()
        .context("启动更新程序失败")?;
    Ok(())
}

fn update_script(pid: u32, archive: &Path, app_dir: &Path, update_root: &Path) -> String {
    let quote = |path: &Path| path.to_string_lossy().replace('\'', "''");
    format!(
        r#"$ErrorActionPreference = "Stop"
$pidToWait = {pid}
$archive = '{archive}'
$appDir = '{app_dir}'
$updateRoot = '{update_root}'
$payload = Join-Path $updateRoot "payload"
Wait-Process -Id $pidToWait -ErrorAction SilentlyContinue
Remove-Item $payload -Recurse -Force -ErrorAction SilentlyContinue
Expand-Archive -Path $archive -DestinationPath $payload -Force
$exe = Get-ChildItem $payload -Filter "ScreenTranslator.exe" -Recurse | Select-Object -First 1
if (-not $exe) {{ throw "更新包中缺少 ScreenTranslator.exe" }}
$sourceRoot = $exe.Directory.FullName
Copy-Item (Join-Path $sourceRoot "*") $appDir -Recurse -Force
Start-Process (Join-Path $appDir "ScreenTranslator.exe")
Start-Sleep -Seconds 2
Remove-Item $updateRoot -Recurse -Force -ErrorAction SilentlyContinue
"#,
        archive = quote(archive),
        app_dir = quote(app_dir),
        update_root = quote(update_root),
    )
}

fn is_newer(candidate: &str, current: &str) -> bool {
    version_parts(candidate) > version_parts(current)
}

fn version_parts(version: &str) -> (u64, u64, u64, String) {
    let version = version.trim_start_matches(['v', 'V']);
    let (core, suffix) = version.split_once('-').unwrap_or((version, ""));
    let mut parts = core.split('.').map(|part| part.parse::<u64>().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        suffix.to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_release_versions() {
        assert!(is_newer("v1.2.0", "1.1.9"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("0.9.9", "1.0.0"));
    }

    #[test]
    fn script_quotes_paths_and_waits_for_process() {
        let script = update_script(
            42,
            &std::path::PathBuf::from(r"C:\Temp\a'b.zip"),
            &std::path::PathBuf::from(r"C:\Program Files\ScreenTranslator"),
            &std::path::PathBuf::from(r"C:\Temp\update"),
        );
        assert!(script.contains("$pidToWait = 42"));
        assert!(script.contains("a''b.zip"));
        assert!(script.contains("Wait-Process"));
    }
}
