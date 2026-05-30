use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kimetsu_core::KimetsuResult;
use reqwest::blocking::Client;
use serde::Deserialize;

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/RodCor/kimetsu/releases/latest";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateFlavor {
    Auto,
    Lean,
    Embeddings,
}

impl UpdateFlavor {
    pub fn parse(value: &str) -> KimetsuResult<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "lean" => Ok(Self::Lean),
            "embeddings" | "semantic" => Ok(Self::Embeddings),
            other => Err(
                format!("unknown update flavor `{other}`; use auto, lean, or embeddings").into(),
            ),
        }
    }

    fn resolve(self) -> &'static str {
        match self {
            Self::Auto => default_flavor(),
            Self::Lean => "lean",
            Self::Embeddings => "embeddings",
        }
    }
}

#[derive(Debug, Clone)]
pub struct UpdateOptions {
    pub check: bool,
    pub dry_run: bool,
    pub force: bool,
    pub flavor: UpdateFlavor,
}

#[derive(Debug, Clone)]
pub struct UninstallOptions {
    pub dry_run: bool,
    pub yes: bool,
    pub delete_user_data: bool,
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<GitHubAsset>,
}

impl GitHubRelease {
    fn version(&self) -> &str {
        self.tag_name.trim_start_matches('v')
    }
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone)]
struct Installation {
    path: PathBuf,
    source: InstallSource,
    version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallSource {
    CurrentExe,
    Path,
    CargoBin,
    StandardBin,
}

impl InstallSource {
    fn label(self) -> &'static str {
        match self {
            Self::CurrentExe => "current executable",
            Self::Path => "PATH",
            Self::CargoBin => "Cargo bin",
            Self::StandardBin => "standard bin",
        }
    }
}

pub fn run(options: UpdateOptions) -> KimetsuResult<()> {
    let flavor = options.flavor.resolve();
    let release = fetch_latest_release()?;
    let latest = release.version();
    let current = Version::parse(CURRENT_VERSION)
        .ok_or_else(|| format!("current kimetsu version is not semver-like: {CURRENT_VERSION}"))?;
    let latest_parsed = Version::parse(latest).ok_or_else(|| {
        format!(
            "latest release tag is not semver-like: {}",
            release.tag_name
        )
    })?;
    let target = release_target().ok_or_else(|| {
        format!(
            "no prebuilt Kimetsu release target for {} {}; install with `cargo install kimetsu-cli --force`",
            env::consts::OS,
            env::consts::ARCH
        )
    })?;
    let asset = select_asset(&release.assets, target, flavor).ok_or_else(|| {
        format!(
            "latest release {} has no `{target}` `{flavor}` asset; see {}",
            release.tag_name, release.html_url
        )
    })?;

    println!("current: kimetsu {CURRENT_VERSION}");
    println!("latest:  kimetsu {latest}");
    println!("target:  {target} ({flavor})");

    if latest_parsed < current && !options.force {
        println!("status:  local build is newer than the latest published release");
        return Ok(());
    }

    if latest_parsed == current && !options.force {
        println!("status:  up to date");
        return Ok(());
    }

    println!("status:  update available");
    println!("release: {}", release.html_url);

    let installs = discover_installations();
    if installs.is_empty() {
        println!("found:   no installed kimetsu executables in known locations");
    } else {
        println!("found:   {} kimetsu executable(s)", installs.len());
        for install in &installs {
            let version = install.version.as_deref().unwrap_or("unknown");
            println!(
                "  {}  [{}]  {version}",
                install.path.display(),
                install.source.label()
            );
        }
    }

    if options.check {
        println!(
            "next:    run `kimetsu update` to install {}",
            release.tag_name
        );
        return Ok(());
    }

    if installs.is_empty() {
        return Err("no Kimetsu executable found to update".into());
    }

    if options.dry_run {
        println!("dry-run: would download {}", asset.name);
        for install in installs {
            println!("dry-run: would update {}", install.path.display());
        }
        return Ok(());
    }

    let workdir = make_temp_dir("kimetsu-update")?;
    let archive_path = workdir.join(&asset.name);
    download_asset(&asset.browser_download_url, &archive_path)?;
    let new_binary = extract_binary(&archive_path, &workdir.join("extract"))?;

    let mut updated = 0usize;
    let mut failed = Vec::new();
    for install in installs {
        match replace_installation(&new_binary, &install.path) {
            Ok(ReplaceOutcome::Updated) => {
                updated += 1;
                println!("updated: {}", install.path.display());
            }
            Ok(ReplaceOutcome::Scheduled) => {
                updated += 1;
                println!(
                    "scheduled: {} (replacement completes after this process exits)",
                    install.path.display()
                );
            }
            Err(err) => {
                println!("failed:  {} ({err})", install.path.display());
                failed.push(install.path);
            }
        }
    }

    let _ = fs::remove_dir_all(&workdir);

    if failed.is_empty() {
        println!("done:    updated {updated} Kimetsu executable(s)");
        Ok(())
    } else {
        Err(format!(
            "updated {updated} Kimetsu executable(s), but {} location(s) failed; rerun from an elevated shell if needed",
            failed.len()
        )
        .into())
    }
}

pub fn uninstall(options: UninstallOptions) -> KimetsuResult<()> {
    let installs = discover_installations();
    if installs.is_empty() {
        println!("found:   no installed kimetsu executables in known locations");
    } else {
        println!("found:   {} kimetsu executable(s)", installs.len());
        for install in &installs {
            let version = install.version.as_deref().unwrap_or("unknown");
            println!(
                "  {}  [{}]  {version}",
                install.path.display(),
                install.source.label()
            );
        }
    }

    if let Some(user_data) = user_data_dir()
        && options.delete_user_data
    {
        println!("user-data: {}", user_data.display());
    }

    if options.dry_run {
        for install in installs {
            println!("dry-run: would remove {}", install.path.display());
        }
        if let Some(user_data) = user_data_dir()
            && options.delete_user_data
        {
            println!("dry-run: would remove user data {}", user_data.display());
        }
        return Ok(());
    }

    if !options.yes {
        return Err(
            "refusing to uninstall without confirmation; rerun with `kimetsu uninstall --yes`"
                .into(),
        );
    }

    let mut removed = 0usize;
    let mut failed = Vec::new();
    for install in installs {
        match remove_installation(&install.path) {
            Ok(RemoveOutcome::Removed) => {
                removed += 1;
                println!("removed: {}", install.path.display());
            }
            Ok(RemoveOutcome::Scheduled) => {
                removed += 1;
                println!(
                    "scheduled: {} (removal completes after this process exits)",
                    install.path.display()
                );
            }
            Err(err) => {
                println!("failed:  {} ({err})", install.path.display());
                failed.push(install.path);
            }
        }
    }

    if options.delete_user_data
        && let Some(user_data) = user_data_dir()
        && user_data.exists()
    {
        match fs::remove_dir_all(&user_data) {
            Ok(()) => println!("removed user data: {}", user_data.display()),
            Err(err) => {
                println!("failed user data: {} ({err})", user_data.display());
                failed.push(user_data);
            }
        }
    }

    if failed.is_empty() {
        println!("done:    removed {removed} Kimetsu executable(s)");
        Ok(())
    } else {
        Err(format!(
            "removed {removed} Kimetsu executable(s), but {} location(s) failed; rerun from an elevated shell if needed",
            failed.len()
        )
        .into())
    }
}

fn fetch_latest_release() -> KimetsuResult<GitHubRelease> {
    let client = Client::builder().timeout(Duration::from_secs(20)).build()?;
    let response = client
        .get(LATEST_RELEASE_URL)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", format!("kimetsu/{CURRENT_VERSION}"))
        .send()?
        .error_for_status()?;
    Ok(response.json()?)
}

fn download_asset(url: &str, target: &Path) -> KimetsuResult<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    let mut response = client
        .get(url)
        .header("Accept", "application/octet-stream")
        .header("User-Agent", format!("kimetsu/{CURRENT_VERSION}"))
        .send()?
        .error_for_status()?;
    let mut file = fs::File::create(target)?;
    io::copy(&mut response, &mut file)?;
    Ok(())
}

fn extract_binary(archive: &Path, dest: &Path) -> KimetsuResult<PathBuf> {
    fs::create_dir_all(dest)?;
    if archive.extension().and_then(|s| s.to_str()) == Some("zip") {
        extract_zip(archive, dest)?;
    } else {
        extract_tar_gz(archive, dest)?;
    }
    find_binary_under(dest).ok_or_else(|| {
        format!(
            "archive {} did not contain {}",
            archive.display(),
            binary_name()
        )
        .into()
    })
}

fn extract_zip(archive: &Path, dest: &Path) -> KimetsuResult<()> {
    let archive = ps_quote(archive);
    let dest = ps_quote(dest);
    let script = format!("Expand-Archive -LiteralPath {archive} -DestinationPath {dest} -Force");
    let status = ProcessCommand::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err("failed to extract release zip with PowerShell Expand-Archive".into())
    }
}

fn extract_tar_gz(archive: &Path, dest: &Path) -> KimetsuResult<()> {
    let status = ProcessCommand::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err("failed to extract release tarball with tar".into())
    }
}

fn discover_installations() -> Vec<Installation> {
    let mut candidates = BTreeMap::<PathBuf, InstallSource>::new();
    if let Ok(current) = env::current_exe() {
        insert_candidate(&mut candidates, current, InstallSource::CurrentExe);
    }
    for path in path_candidates() {
        insert_candidate(&mut candidates, path, InstallSource::Path);
    }
    for path in cargo_bin_candidates() {
        insert_candidate(&mut candidates, path, InstallSource::CargoBin);
    }
    for path in standard_bin_candidates() {
        insert_candidate(&mut candidates, path, InstallSource::StandardBin);
    }

    candidates
        .into_iter()
        .filter_map(|(path, source)| {
            if !path.exists() {
                return None;
            }
            let version = kimetsu_version_at(&path)?;
            Some(Installation {
                path,
                source,
                version: Some(version),
            })
        })
        .collect()
}

fn insert_candidate(
    candidates: &mut BTreeMap<PathBuf, InstallSource>,
    path: PathBuf,
    source: InstallSource,
) {
    let key = path.canonicalize().unwrap_or(path);
    candidates.entry(key).or_insert(source);
}

fn path_candidates() -> Vec<PathBuf> {
    let Some(path_var) = env::var_os("PATH") else {
        return Vec::new();
    };
    env::split_paths(&path_var)
        .map(|dir| dir.join(binary_name()))
        .collect()
}

fn cargo_bin_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(cargo_home) = env::var_os("CARGO_HOME") {
        out.push(PathBuf::from(cargo_home).join("bin").join(binary_name()));
    }
    if let Some(home) = home_dir() {
        out.push(home.join(".cargo").join("bin").join(binary_name()));
    }
    out
}

fn standard_bin_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if cfg!(windows) {
        if let Some(profile) = env::var_os("USERPROFILE") {
            out.push(
                PathBuf::from(profile)
                    .join(".cargo")
                    .join("bin")
                    .join(binary_name()),
            );
        }
    } else {
        if let Some(home) = home_dir() {
            out.push(home.join(".local").join("bin").join(binary_name()));
        }
        out.push(PathBuf::from("/usr/local/bin").join(binary_name()));
        out.push(PathBuf::from("/opt/homebrew/bin").join(binary_name()));
    }
    out
}

fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        env::var_os("USERPROFILE").map(PathBuf::from)
    } else {
        env::var_os("HOME").map(PathBuf::from)
    }
}

fn user_data_dir() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("KIMETSU_USER_BRAIN_DIR") {
        return Some(PathBuf::from(dir));
    }
    home_dir().map(|home| home.join(".kimetsu"))
}

fn kimetsu_version_at(path: &Path) -> Option<String> {
    let output = ProcessCommand::new(path).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let text = stdout.trim();
    if !text.starts_with("kimetsu ") {
        return None;
    }
    Some(text.trim_start_matches("kimetsu ").to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplaceOutcome {
    Updated,
    Scheduled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoveOutcome {
    Removed,
    Scheduled,
}

fn replace_installation(source: &Path, target: &Path) -> KimetsuResult<ReplaceOutcome> {
    #[cfg(windows)]
    {
        if is_current_exe(target) {
            return schedule_windows_self_replace(source, target);
        }
        fs::copy(source, target)?;
        Ok(ReplaceOutcome::Updated)
    }
    #[cfg(not(windows))]
    {
        atomic_replace(source, target)?;
        Ok(ReplaceOutcome::Updated)
    }
}

fn remove_installation(target: &Path) -> KimetsuResult<RemoveOutcome> {
    #[cfg(windows)]
    {
        if is_current_exe(target) {
            return schedule_windows_self_delete(target);
        }
    }
    fs::remove_file(target)?;
    Ok(RemoveOutcome::Removed)
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, target: &Path) -> KimetsuResult<()> {
    let tmp = target.with_file_name(format!(
        "{}.kimetsu-update-{}",
        target
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("kimetsu"),
        std::process::id()
    ));
    fs::copy(source, &tmp)?;
    mark_executable(&tmp)?;
    fs::rename(&tmp, target)?;
    Ok(())
}

#[cfg(windows)]
fn schedule_windows_self_replace(source: &Path, target: &Path) -> KimetsuResult<ReplaceOutcome> {
    let staged = target.with_file_name(format!(
        "{}.kimetsu-update-{}",
        target
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("kimetsu.exe"),
        std::process::id()
    ));
    fs::copy(source, &staged)?;
    let script = format!(
        "$pidToWait = {pid}; \
         $src = {src}; \
         $dst = {dst}; \
         while (Get-Process -Id $pidToWait -ErrorAction SilentlyContinue) {{ Start-Sleep -Milliseconds 200 }}; \
         Move-Item -LiteralPath $src -Destination $dst -Force",
        pid = std::process::id(),
        src = ps_quote(&staged),
        dst = ps_quote(target),
    );
    ProcessCommand::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &script,
        ])
        .spawn()?;
    Ok(ReplaceOutcome::Scheduled)
}

#[cfg(windows)]
fn schedule_windows_self_delete(target: &Path) -> KimetsuResult<RemoveOutcome> {
    let script = format!(
        "$pidToWait = {pid}; \
         $dst = {dst}; \
         while (Get-Process -Id $pidToWait -ErrorAction SilentlyContinue) {{ Start-Sleep -Milliseconds 200 }}; \
         Remove-Item -LiteralPath $dst -Force",
        pid = std::process::id(),
        dst = ps_quote(target),
    );
    ProcessCommand::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &script,
        ])
        .spawn()?;
    Ok(RemoveOutcome::Scheduled)
}

#[cfg(windows)]
fn is_current_exe(path: &Path) -> bool {
    let Ok(current) = env::current_exe().and_then(|p| p.canonicalize()) else {
        return false;
    };
    path.canonicalize()
        .map(|target| target == current)
        .unwrap_or(false)
}

#[cfg(unix)]
fn mark_executable(path: &Path) -> KimetsuResult<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(all(not(unix), not(windows)))]
fn mark_executable(_path: &Path) -> KimetsuResult<()> {
    Ok(())
}

fn find_binary_under(root: &Path) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let entries = fs::read_dir(path).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|s| s.to_str()) == Some(binary_name()) {
                return Some(path);
            }
        }
    }
    None
}

fn make_temp_dir(prefix: &str) -> KimetsuResult<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn select_asset<'a>(
    assets: &'a [GitHubAsset],
    target: &str,
    flavor: &str,
) -> Option<&'a GitHubAsset> {
    assets.iter().find(|asset| {
        asset.name.contains(target)
            && asset.name.contains(flavor)
            && (asset.name.ends_with(".tar.gz") || asset.name.ends_with(".zip"))
    })
}

fn default_flavor() -> &'static str {
    if cfg!(feature = "embeddings") {
        "embeddings"
    } else {
        "lean"
    }
}

fn release_target() -> Option<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

fn binary_name() -> &'static str {
    if cfg!(windows) {
        "kimetsu.exe"
    } else {
        "kimetsu"
    }
}

fn ps_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "''"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    fn parse(value: &str) -> Option<Self> {
        let clean = value.trim().trim_start_matches('v');
        let numeric = clean.split_once('-').map(|(head, _)| head).unwrap_or(clean);
        let mut parts = numeric.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().ok()?;
        let patch = parts.next().unwrap_or("0").parse().ok()?;
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare_handles_multi_digit_minor() {
        assert!(Version::parse("0.10.0") > Version::parse("0.9.9"));
        assert!(Version::parse("v1.2.3") > Version::parse("1.2.2"));
    }

    #[test]
    fn select_asset_matches_target_and_flavor() {
        let assets = vec![
            GitHubAsset {
                name: "kimetsu-0.7.3-x86_64-pc-windows-msvc-lean.zip".into(),
                browser_download_url: "https://example.invalid/lean.zip".into(),
            },
            GitHubAsset {
                name: "kimetsu-0.7.3-x86_64-pc-windows-msvc-embeddings.zip".into(),
                browser_download_url: "https://example.invalid/embeddings.zip".into(),
            },
        ];

        let asset = select_asset(&assets, "x86_64-pc-windows-msvc", "embeddings").expect("asset");
        assert_eq!(
            asset.name,
            "kimetsu-0.7.3-x86_64-pc-windows-msvc-embeddings.zip"
        );
    }
}
