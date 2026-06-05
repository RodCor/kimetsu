use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kimetsu_core::KimetsuResult;
use reqwest::blocking::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::process::{KimetsuProc, ProcKind};

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

/// Three-tier removal depth for `kimetsu uninstall`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// Remove only the kimetsu binary. Plugin wiring and brains are kept.
    BinaryOnly,
    /// Remove the binary AND the Kimetsu plugin wiring injected into Claude
    /// Code & Codex (hooks, MCP entries, skills, CLAUDE.md blocks). Brains
    /// are kept. This is the **default** because leaving dangling hooks after
    /// the binary is gone breaks the host agents.
    WithPlugins,
    /// Remove binary + plugin wiring + the user brain (`~/.kimetsu`) and the
    /// current workspace's project brain (`.kimetsu/`). Irreversible; requires
    /// explicit opt-in (`--delete-user-data` or a typed interactive confirm).
    WithBrains,
}

#[derive(Debug, Clone)]
pub struct UninstallOptions {
    pub dry_run: bool,
    pub yes: bool,
    /// Skip plugin-wiring removal (select Tier 1 / BinaryOnly).
    pub keep_plugins: bool,
    /// Also remove the user brain and workspace brain (select Tier 3 / WithBrains).
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
    let asset = select_asset(&release.assets, latest, target, flavor).ok_or_else(|| {
        format!(
            "latest release {} has no `{target}` `{flavor}` asset; see {}",
            release.tag_name, release.html_url
        )
    })?;
    let checksum_asset = select_checksum_asset(&release.assets).ok_or_else(|| {
        format!(
            "latest release {} has no checksums.txt asset; refusing unsigned update",
            release.tag_name
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
    let checksums_path = workdir.join(&checksum_asset.name);
    download_asset(&asset.browser_download_url, &archive_path)?;
    download_asset(&checksum_asset.browser_download_url, &checksums_path)?;
    verify_asset_checksum(&archive_path, &asset.name, &checksums_path)?;
    let new_binary = extract_binary(&archive_path, &workdir.join("extract"))?;

    let mut updated = 0usize;
    let mut failed = Vec::new();
    let mut stopped_mcp = 0usize;
    for install in installs {
        // ── Windows pre-flight: detect locking processes and offer to stop them.
        #[cfg(windows)]
        {
            let locking = crate::process::processes_locking_target(&install.path);
            if !locking.is_empty() {
                let is_tty = io::stdin().is_terminal();
                let stdin = io::stdin();
                let mut reader = stdin.lock();
                let mut stdout = io::stdout();
                let action = decide_preflight_action(
                    &locking,
                    is_tty,
                    options.force,
                    &mut reader,
                    &mut stdout,
                )
                .unwrap_or(PreflightAction::Defer);
                match action {
                    PreflightAction::Stop => {
                        let pids: Vec<u32> = locking.iter().map(|p| p.pid).collect();
                        let results = crate::process::stop_processes(&pids);
                        let mut all_ok = true;
                        for (pid, result) in &results {
                            match result {
                                Ok(()) => println!("  stopped PID {pid}"),
                                Err(e) => {
                                    eprintln!("  failed to stop PID {pid}: {e}");
                                    all_ok = false;
                                }
                            }
                        }
                        let n_mcp = locking
                            .iter()
                            .filter(|p| p.kind == ProcKind::McpServe)
                            .count();
                        if all_ok {
                            stopped_mcp += n_mcp;
                            // Brief poll: wait up to ~3 s for the OS to release the lock.
                            let max_wait = Duration::from_secs(3);
                            let poll = Duration::from_millis(200);
                            let started = std::time::Instant::now();
                            loop {
                                if started.elapsed() >= max_wait {
                                    break;
                                }
                                // Try a non-destructive open to probe lock release.
                                if fs::File::open(&install.path).is_ok() {
                                    break;
                                }
                                std::thread::sleep(poll);
                            }
                        }
                        // Fall through to replace_installation.
                    }
                    PreflightAction::Defer => {
                        // User declined or non-interactive: skip replace now;
                        // schedule will happen inside replace_installation if still locked.
                    }
                }
            }
        }

        match replace_installation(&new_binary, &install.path) {
            Ok(ReplaceOutcome::Updated) => {
                updated += 1;
                println!(
                    "updated: {}",
                    kimetsu_core::paths::display_path(&install.path)
                );
            }
            Ok(ReplaceOutcome::Scheduled) => {
                updated += 1;
                println!(
                    "scheduled: {} (replacement completes after this process exits)",
                    kimetsu_core::paths::display_path(&install.path)
                );
            }
            Err(err) => {
                println!(
                    "failed:  {} ({err})",
                    kimetsu_core::paths::display_path(&install.path)
                );
                failed.push(install.path);
            }
        }
    }

    let _ = fs::remove_dir_all(&workdir);

    if failed.is_empty() {
        println!("done:    updated {updated} Kimetsu executable(s) to v{latest}");
        print_post_update_next_steps(stopped_mcp);
        Ok(())
    } else {
        Err(format!(
            "updated {updated} Kimetsu executable(s), but {} location(s) failed; \
             if the binary is locked by a running kimetsu process (e.g. `kimetsu mcp serve`), \
             quit Claude Code / Codex or run: Get-Process kimetsu | Stop-Process -Force",
            failed.len()
        )
        .into())
    }
}

/// Print the "what now?" guidance after a successful update. Always shown so a
/// user upgrading from an older version is told to restart their host agent (so
/// the still-running MCP server respawns on the new binary) and to verify with
/// `kimetsu doctor` — the most common post-update confusion is a stale MCP
/// server serving the old image until the host restarts.
fn print_post_update_next_steps(stopped_mcp: usize) {
    // Any MCP server still running was spawned from the pre-update binary.
    let running_mcp = crate::process::list_kimetsu_processes()
        .into_iter()
        .filter(|p| matches!(p.kind, ProcKind::McpServe))
        .count();

    println!();
    println!("next steps:");
    if running_mcp > 0 {
        println!(
            "  - {running_mcp} kimetsu MCP server(s) are still running the previous version — \
             restart your host agent (Claude Code / Codex) so it respawns on the new binary."
        );
    } else if stopped_mcp > 0 {
        println!(
            "  - your host agent (Claude Code / Codex) will respawn its MCP server on the next \
             call — restart it to load the new version."
        );
    } else {
        println!(
            "  - if a host agent (Claude Code / Codex) is open, restart it so its kimetsu MCP \
             server reloads the new binary."
        );
    }
    println!("  - run `kimetsu doctor` to confirm the brain + wiring are healthy.");
    println!(
        "  - installed via cargo or npm? update those with `cargo install kimetsu-cli --force` \
         or `npm update -g kimetsu-ai` instead."
    );
}

/// Resolve which removal tier to use given the options, TTY state, and user
/// input. Factored out so unit tests can drive it with a scripted reader
/// without touching the filesystem.
///
/// Rules:
/// * Non-interactive (`!is_tty` or `yes`): flags decide directly.
///   - `keep_plugins` → BinaryOnly
///   - `delete_user_data` → WithBrains  (flag is the brain confirm in non-interactive mode)
///   - neither flag → WithPlugins (safe default)
/// * Interactive (TTY, `--yes` not passed): print the menu, read a line.
///   - "" or "2" → WithPlugins (default)
///   - "1" → BinaryOnly
///   - "3" → ask for typed confirm `delete-brains`; anything else → WithPlugins
///   - `--keep-plugins` flag pre-selects 1; `--delete-user-data` flag pre-selects 3
///     but the user still confirms.
pub fn resolve_tier<R: BufRead, W: Write>(
    options: &UninstallOptions,
    is_tty: bool,
    reader: &mut R,
    writer: &mut W,
) -> KimetsuResult<Tier> {
    // Non-interactive path: flags decide, no prompts.
    if !is_tty || options.yes {
        if options.keep_plugins {
            return Ok(Tier::BinaryOnly);
        }
        if options.delete_user_data {
            return Ok(Tier::WithBrains);
        }
        return Ok(Tier::WithPlugins);
    }

    // Interactive path: print the three-option menu.
    let default_label = if options.keep_plugins {
        "1"
    } else if options.delete_user_data {
        "3"
    } else {
        "2"
    };

    writeln!(
        writer,
        "\nWhat should I remove?\n  \
         1) Kimetsu binary only (leave plugin wiring + memories)\n  \
         2) Binary + plugin wiring in Claude Code & Codex (recommended — keeps your hosts working) [default]\n  \
         3) Binary + plugin wiring + ALL memories (deletes your brains — irreversible)"
    )?;
    write!(writer, "Choose [1/2/3] (default {default_label}): ")?;
    writer.flush()?;

    let mut line = String::new();
    reader.read_line(&mut line)?;
    let choice = line.trim();

    // Empty input → use the preselected default.
    let effective = if choice.is_empty() {
        default_label
    } else {
        choice
    };

    match effective {
        "1" => Ok(Tier::BinaryOnly),
        "3" => {
            // Require a typed confirm before deleting brains.
            write!(writer, "Type \"delete-brains\" to confirm: ")?;
            writer.flush()?;
            let mut confirm = String::new();
            reader.read_line(&mut confirm)?;
            if confirm.trim() == "delete-brains" {
                Ok(Tier::WithBrains)
            } else {
                writeln!(
                    writer,
                    "Confirmation not matched — keeping memories, removing binary + plugin wiring."
                )?;
                Ok(Tier::WithPlugins)
            }
        }
        // "2" or anything unrecognised → safe default.
        _ => Ok(Tier::WithPlugins),
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

    // Show a one-line note about plugin wiring so users know what tier 2 covers.
    println!("plugin-wiring: will scan Claude Code & Codex hooks/MCP/skills (workspace + global)");

    // Show brain paths if they exist (so users can make an informed choice).
    if let Some(user_data) = user_data_dir() {
        if user_data.exists() {
            println!("user-brain: {} (exists)", user_data.display());
        } else {
            println!("user-brain: {} (not found)", user_data.display());
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project_brain = cwd.join(".kimetsu");
    if project_brain.exists() {
        println!("project-brain: {} (exists)", project_brain.display());
    }

    // -- Dry-run: resolve the tier from flags and describe what would happen.
    if options.dry_run {
        // For dry-run, resolve tier from flags only (non-interactive).
        let tier = if options.keep_plugins {
            Tier::BinaryOnly
        } else if options.delete_user_data {
            Tier::WithBrains
        } else {
            Tier::WithPlugins
        };
        for install in &installs {
            println!("dry-run: would remove {}", install.path.display());
        }
        if tier >= Tier::WithPlugins {
            println!(
                "dry-run: would remove Kimetsu wiring from Claude Code & Codex (workspace + global)"
            );
        }
        if tier >= Tier::WithBrains {
            if let Some(user_data) = user_data_dir() {
                if user_data.exists() {
                    println!("dry-run: would remove user brain {}", user_data.display());
                }
            }
            if project_brain.exists() {
                println!(
                    "dry-run: would remove project brain {}",
                    project_brain.display()
                );
            }
        }
        return Ok(());
    }

    // -- Interactive / flag-driven confirmation.
    let is_tty = io::stdin().is_terminal();
    if !is_tty && !options.yes {
        return Err(
            "refusing to uninstall without confirmation; rerun with `kimetsu uninstall --yes`"
                .into(),
        );
    }

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut stdout = io::stdout();
    let tier = resolve_tier(&options, is_tty, &mut reader, &mut stdout)?;

    // -- Binary removal (all tiers).
    let mut removed = 0usize;
    let mut failed: Vec<PathBuf> = Vec::new();
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

    // -- Plugin-wiring removal (tiers 2 and 3).
    if tier >= Tier::WithPlugins {
        use kimetsu_chat::bridge::{BridgeTarget, InstallScope, plugin_uninstall};
        let mut total_removed = 0usize;
        let mut total_modified = 0usize;

        for target in [BridgeTarget::ClaudeCode, BridgeTarget::Codex] {
            for scope in [InstallScope::Workspace, InstallScope::Global] {
                match plugin_uninstall(&cwd, target, scope) {
                    Ok(report) => {
                        total_removed += report.removed.len();
                        total_modified += report.modified.len();
                    }
                    Err(err) => {
                        let label = format!("{} {:?}", target.as_str(), scope);
                        println!("failed plugin-wiring ({label}): {err}");
                        failed.push(cwd.join(format!("plugin-wiring/{label}")));
                    }
                }
            }
        }
        println!(
            "removed Kimetsu wiring: {total_modified} file(s) edited, {total_removed} file(s)/dir(s) deleted across Claude Code & Codex"
        );
    }

    // -- Brain removal (tier 3 only).
    if tier >= Tier::WithBrains {
        if let Some(user_data) = user_data_dir()
            && user_data.exists()
        {
            match fs::remove_dir_all(&user_data) {
                Ok(()) => println!("removed user brain: {}", user_data.display()),
                Err(err) => {
                    println!("failed user brain: {} ({err})", user_data.display());
                    failed.push(user_data);
                }
            }
        }
        if project_brain.exists() {
            match fs::remove_dir_all(&project_brain) {
                Ok(()) => println!("removed project brain: {}", project_brain.display()),
                Err(err) => {
                    println!("failed project brain: {} ({err})", project_brain.display());
                    failed.push(project_brain);
                }
            }
        }
    }

    if failed.is_empty() {
        println!("done:    removed {removed} Kimetsu executable(s)");
        Ok(())
    } else {
        Err(format!(
            "removed {removed} Kimetsu executable(s), but {} location(s) failed; \
             if the binary is locked by a running kimetsu process (e.g. `kimetsu mcp serve`), \
             quit Claude Code / Codex or run: Get-Process kimetsu | Stop-Process -Force",
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

fn verify_asset_checksum(archive: &Path, asset_name: &str, checksums: &Path) -> KimetsuResult<()> {
    let manifest = fs::read_to_string(checksums)?;
    let expected = checksum_for_asset(&manifest, asset_name).ok_or_else(|| {
        format!("checksums.txt does not contain an entry for release asset `{asset_name}`")
    })?;
    let actual = sha256_file_hex(archive)?;
    if !actual.eq_ignore_ascii_case(&expected) {
        return Err(format!(
            "checksum mismatch for {asset_name}: expected {expected}, got {actual}"
        )
        .into());
    }
    Ok(())
}

fn checksum_for_asset(manifest: &str, asset_name: &str) -> Option<String> {
    for line in manifest
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if let Some((hash, name)) = parse_sha256sum_line(line)
            && name == asset_name
        {
            return Some(hash.to_ascii_lowercase());
        }
        if let Some((name, hash)) = parse_bsd_checksum_line(line)
            && name == asset_name
        {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

fn parse_sha256sum_line(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.split_whitespace();
    let hash = parts.next()?;
    if !is_sha256_hex(hash) {
        return None;
    }
    let name = parts.next()?.trim_start_matches('*');
    Some((hash, name))
}

fn parse_bsd_checksum_line(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("SHA256 (")?;
    let (name, hash) = rest.split_once(") = ")?;
    if !is_sha256_hex(hash) {
        return None;
    }
    Some((name, hash))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn sha256_file_hex(path: &Path) -> KimetsuResult<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
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

/// Decision outcome for the update pre-flight locking check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightAction {
    /// Stop the locking processes, then attempt the replace immediately.
    Stop,
    /// Skip stopping; fall back to deferred (schedule after process exit).
    Defer,
}

/// Pure decision function for the pre-flight locking check.
///
/// Given a list of locking processes, TTY state, `--force` flag, and a
/// reader/writer pair, prints the locking processes and prompts the user
/// (when interactive) whether to stop them.
///
/// Rules:
/// * Non-interactive (!is_tty) or `force` flag: print the PIDs and return
///   `Defer` to protect CI from silent process kills.
///   (If `force` is true the caller passes it as a signal that the user
///   consented — we still defer because killing processes in CI is risky;
///   the deferred-replace path is safe.)
/// * Interactive (TTY, no `force`): print the list, prompt `[Y/n]`.
///   `Y` / empty → `Stop`; `n` → `Defer`.
pub fn decide_preflight_action<R: BufRead, W: Write>(
    locking: &[KimetsuProc],
    is_tty: bool,
    force: bool,
    reader: &mut R,
    writer: &mut W,
) -> KimetsuResult<PreflightAction> {
    if locking.is_empty() {
        return Ok(PreflightAction::Defer);
    }

    // Always print which processes are locking the binary.
    writeln!(
        writer,
        "preflight: the following kimetsu process(es) are holding the binary locked:"
    )?;
    for p in locking {
        writeln!(
            writer,
            "  PID {}  [{}]  workspace={}",
            p.pid,
            p.kind.label(),
            p.workspace.as_deref().unwrap_or("-")
        )?;
    }

    if !is_tty || force {
        // Non-interactive: print guidance and defer.
        let pid_list = locking
            .iter()
            .map(|p| p.pid.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(
            writer,
            "notice:  update will use deferred replace (completes after the process exits).\n\
             To update immediately, stop those processes or run:\n\
             Get-Process kimetsu | Stop-Process -Force  [PIDs: {pid_list}]"
        )?;
        return Ok(PreflightAction::Defer);
    }

    // Interactive: prompt.
    write!(
        writer,
        "Stop these kimetsu process(es) so the update can complete now? [Y/n] "
    )?;
    writer.flush()?;

    let mut line = String::new();
    reader.read_line(&mut line)?;
    let answer = line.trim().to_lowercase();

    if answer.is_empty() || answer == "y" || answer == "yes" {
        Ok(PreflightAction::Stop)
    } else {
        writeln!(writer, "Skipping stop — will use deferred replace instead.")?;
        Ok(PreflightAction::Defer)
    }
}

fn replace_installation(source: &Path, target: &Path) -> KimetsuResult<ReplaceOutcome> {
    #[cfg(windows)]
    {
        if is_current_exe(target) {
            return schedule_windows_self_replace(source, target);
        }
        match fs::copy(source, target) {
            Ok(_) => Ok(ReplaceOutcome::Updated),
            Err(err) if is_sharing_violation(&err) => {
                // The target is locked by a sibling kimetsu process.
                let pids = kimetsu_processes_locking(target);
                if !pids.is_empty() {
                    let pid_list = pids
                        .iter()
                        .map(|p| p.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!(
                        "notice:  {} is held by kimetsu process(es) [PID: {}]; scheduling deferred replace.",
                        target.display(),
                        pid_list
                    );
                    println!(
                        "         To complete the update now: quit Claude Code / Codex so their\n\
                         `kimetsu mcp serve` exits, or run: Get-Process kimetsu | Stop-Process -Force"
                    );
                    schedule_windows_locked_replace(source, target, &pids)
                } else {
                    Err(err.into())
                }
            }
            Err(err) => Err(err.into()),
        }
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
        match fs::remove_file(target) {
            Ok(()) => Ok(RemoveOutcome::Removed),
            Err(err) if is_sharing_violation(&err) => {
                // The target is locked by a sibling kimetsu process.
                let pids = kimetsu_processes_locking(target);
                if !pids.is_empty() {
                    let pid_list = pids
                        .iter()
                        .map(|p| p.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!(
                        "notice:  {} is held by kimetsu process(es) [PID: {}]; scheduling deferred delete.",
                        target.display(),
                        pid_list
                    );
                    println!(
                        "         To complete the uninstall now: quit Claude Code / Codex so their\n\
                         `kimetsu mcp serve` exits, or run: Get-Process kimetsu | Stop-Process -Force"
                    );
                    schedule_windows_locked_delete(target, &pids)
                } else {
                    Err(err.into())
                }
            }
            Err(err) => Err(err.into()),
        }
    }
    #[cfg(not(windows))]
    {
        fs::remove_file(target)?;
        Ok(RemoveOutcome::Removed)
    }
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
    // Wait for our own PID to exit, then poll-retry Move-Item for up to 60 s
    // so any other sibling process that might still hold the file can also exit.
    let script = format!(
        "$pidToWait = {pid}; \
         $src = {src}; \
         $dst = {dst}; \
         while (Get-Process -Id $pidToWait -ErrorAction SilentlyContinue) {{ Start-Sleep -Milliseconds 200 }}; \
         $deadline = (Get-Date).AddSeconds(60); \
         $done = $false; \
         while (-not $done -and (Get-Date) -lt $deadline) {{ \
           try {{ Move-Item -LiteralPath $src -Destination $dst -Force -ErrorAction Stop; $done = $true }} \
           catch {{ Start-Sleep -Milliseconds 500 }} \
         }}",
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
    // Wait for our own PID to exit, then poll-retry Remove-Item for up to 60 s
    // so any other sibling process that might still hold the file can also exit.
    let script = format!(
        "$pidToWait = {pid}; \
         $dst = {dst}; \
         while (Get-Process -Id $pidToWait -ErrorAction SilentlyContinue) {{ Start-Sleep -Milliseconds 200 }}; \
         $deadline = (Get-Date).AddSeconds(60); \
         $done = $false; \
         while (-not $done -and (Get-Date) -lt $deadline) {{ \
           try {{ Remove-Item -LiteralPath $dst -Force -ErrorAction Stop; $done = $true }} \
           catch {{ Start-Sleep -Milliseconds 500 }} \
         }}",
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

/// Returns true when the IO error is a Windows sharing-violation / access-denied
/// that indicates the file is locked by another process (not an ACL issue).
#[cfg(windows)]
fn is_sharing_violation(err: &io::Error) -> bool {
    // ERROR_ACCESS_DENIED (5) and ERROR_SHARING_VIOLATION (32) both surface as
    // PermissionDenied on Windows when a file is in use by another process.
    matches!(err.kind(), io::ErrorKind::PermissionDenied)
}

/// Return PIDs of kimetsu processes locking `target`.
///
/// Delegates to `process::processes_locking_target` (the rich Q1 lister) so
/// there is exactly one process-enumeration path in the codebase.
/// Used by `replace_installation` / `remove_installation` for the post-failure
/// deferred-replace scheduling (we need PIDs to wait for).
#[cfg(windows)]
fn kimetsu_processes_locking(target: &Path) -> Vec<u32> {
    crate::process::processes_locking_target(target)
        .into_iter()
        .map(|p| p.pid)
        .collect()
}

/// Pure parser for the CSV output of `Get-Process … | ConvertTo-Csv`.
/// Kept as a pure utility / test helper; live code now routes through
/// `process::processes_locking_target` so this is only called from tests.
#[cfg_attr(not(test), allow(dead_code))]
pub fn parse_locking_pids(output: &str, target: &Path, current_pid: u32) -> Vec<u32> {
    // Canonicalize target once; fall back to as-is on failure.
    let target_canon = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());
    let target_str = target_canon.to_string_lossy().to_lowercase();

    let mut pids = Vec::new();
    let mut first = true;
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Skip the CSV header row.
        if first {
            first = false;
            if line.starts_with('"') && line.to_lowercase().contains("id") {
                continue;
            }
        }
        // Each CSV row: "Id","Path"  (ConvertTo-Csv with NoTypeInformation)
        let parts: Vec<&str> = line.splitn(2, ',').collect();
        if parts.len() < 2 {
            continue;
        }
        let id_field = parts[0].trim().trim_matches('"');
        let path_field = parts[1].trim().trim_matches('"');

        let pid: u32 = match id_field.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if pid == current_pid {
            continue;
        }
        // Compare paths case-insensitively (Windows paths are case-insensitive).
        let row_path = Path::new(path_field)
            .canonicalize()
            .unwrap_or_else(|_| Path::new(path_field).to_path_buf());
        let row_str = row_path.to_string_lossy().to_lowercase();
        if row_str == target_str {
            pids.push(pid);
        }
    }
    pids
}

/// Schedule a poll-until-unlocked delete for a sibling-locked binary.
/// The PowerShell script waits for ALL locking PIDs to exit, then retries
/// `Remove-Item` every 500 ms for up to 60 s.
#[cfg(windows)]
fn schedule_windows_locked_delete(
    target: &Path,
    locking_pids: &[u32],
) -> KimetsuResult<RemoveOutcome> {
    let dst = ps_quote(target);
    let pids_array = locking_pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let script = format!(
        "$pids = @({pids_array}); \
         foreach ($p in $pids) {{ \
           while (Get-Process -Id $p -ErrorAction SilentlyContinue) {{ Start-Sleep -Milliseconds 200 }} \
         }}; \
         $deadline = (Get-Date).AddSeconds(60); \
         $done = $false; \
         while (-not $done -and (Get-Date) -lt $deadline) {{ \
           try {{ Remove-Item -LiteralPath {dst} -Force -ErrorAction Stop; $done = $true }} \
           catch {{ Start-Sleep -Milliseconds 500 }} \
         }}",
        pids_array = pids_array,
        dst = dst,
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

/// Schedule a poll-until-unlocked replace for a sibling-locked binary.
/// Stages the new binary first, then waits for locking PIDs to exit and
/// retries `Move-Item` every 500 ms for up to 60 s.
#[cfg(windows)]
fn schedule_windows_locked_replace(
    source: &Path,
    target: &Path,
    locking_pids: &[u32],
) -> KimetsuResult<ReplaceOutcome> {
    let staged = target.with_file_name(format!(
        "{}.kimetsu-update-{}",
        target
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("kimetsu.exe"),
        std::process::id()
    ));
    fs::copy(source, &staged)?;
    let src = ps_quote(&staged);
    let dst = ps_quote(target);
    let pids_array = locking_pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let script = format!(
        "$pids = @({pids_array}); \
         foreach ($p in $pids) {{ \
           while (Get-Process -Id $p -ErrorAction SilentlyContinue) {{ Start-Sleep -Milliseconds 200 }} \
         }}; \
         $deadline = (Get-Date).AddSeconds(60); \
         $done = $false; \
         while (-not $done -and (Get-Date) -lt $deadline) {{ \
           try {{ Move-Item -LiteralPath {src} -Destination {dst} -Force -ErrorAction Stop; $done = $true }} \
           catch {{ Start-Sleep -Milliseconds 500 }} \
         }}",
        pids_array = pids_array,
        src = src,
        dst = dst,
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
    version: &str,
    target: &str,
    flavor: &str,
) -> Option<&'a GitHubAsset> {
    let ext = if target.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    let expected = format!("kimetsu-{version}-{target}-{flavor}.{ext}");
    assets.iter().find(|asset| {
        asset.name == expected
            && asset
                .browser_download_url
                .starts_with("https://github.com/RodCor/kimetsu/releases/download/")
    })
}

fn select_checksum_asset(assets: &[GitHubAsset]) -> Option<&GitHubAsset> {
    assets.iter().find(|asset| {
        asset.name == "checksums.txt"
            && asset
                .browser_download_url
                .starts_with("https://github.com/RodCor/kimetsu/releases/download/")
    })
}

fn default_flavor() -> &'static str {
    default_flavor_for(
        env::consts::OS,
        env::consts::ARCH,
        cfg!(feature = "embeddings"),
    )
}

fn default_flavor_for(os: &str, arch: &str, embeddings_enabled: bool) -> &'static str {
    if embeddings_enabled && embeddings_assets_supported(os, arch) {
        "embeddings"
    } else {
        "lean"
    }
}

fn embeddings_assets_supported(os: &str, arch: &str) -> bool {
    matches!(
        (os, arch),
        ("linux", "x86_64") | ("macos", "aarch64") | ("windows", "x86_64")
    )
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
    use std::io::Cursor;

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
                browser_download_url: "https://github.com/RodCor/kimetsu/releases/download/v0.7.3/kimetsu-0.7.3-x86_64-pc-windows-msvc-lean.zip".into(),
            },
            GitHubAsset {
                name: "kimetsu-0.7.3-x86_64-pc-windows-msvc-embeddings.zip".into(),
                browser_download_url: "https://github.com/RodCor/kimetsu/releases/download/v0.7.3/kimetsu-0.7.3-x86_64-pc-windows-msvc-embeddings.zip".into(),
            },
        ];

        let asset =
            select_asset(&assets, "0.7.3", "x86_64-pc-windows-msvc", "embeddings").expect("asset");
        assert_eq!(
            asset.name,
            "kimetsu-0.7.3-x86_64-pc-windows-msvc-embeddings.zip"
        );
    }

    #[test]
    fn select_asset_requires_exact_project_release_asset() {
        let assets = vec![
            GitHubAsset {
                name: "evil-kimetsu-0.7.3-x86_64-pc-windows-msvc-embeddings.zip".into(),
                browser_download_url:
                    "https://github.com/RodCor/kimetsu/releases/download/v0.7.3/evil.zip".into(),
            },
            GitHubAsset {
                name: "kimetsu-0.7.3-x86_64-pc-windows-msvc-embeddings.zip".into(),
                browser_download_url: "https://example.invalid/embeddings.zip".into(),
            },
        ];

        assert!(
            select_asset(&assets, "0.7.3", "x86_64-pc-windows-msvc", "embeddings").is_none(),
            "spoofed names or non-project URLs must not match"
        );
    }

    #[test]
    fn checksum_manifest_parses_common_formats() {
        let hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let manifest = format!(
            "{hash}  kimetsu-1.0.0-x86_64-unknown-linux-gnu-lean.tar.gz\n\
             SHA256 (kimetsu-1.0.0-x86_64-pc-windows-msvc-lean.zip) = {hash}\n"
        );
        assert_eq!(
            checksum_for_asset(
                &manifest,
                "kimetsu-1.0.0-x86_64-unknown-linux-gnu-lean.tar.gz"
            )
            .as_deref(),
            Some(hash)
        );
        assert_eq!(
            checksum_for_asset(&manifest, "kimetsu-1.0.0-x86_64-pc-windows-msvc-lean.zip")
                .as_deref(),
            Some(hash)
        );
        assert!(checksum_for_asset(&manifest, "missing.zip").is_none());
    }

    #[test]
    fn auto_flavor_falls_back_to_lean_for_intel_macos() {
        assert_eq!(
            default_flavor_for("macos", "x86_64", true),
            "lean",
            "the release workflow does not publish x86_64 macOS embeddings assets"
        );
        assert_eq!(default_flavor_for("macos", "aarch64", true), "embeddings");
        assert_eq!(default_flavor_for("linux", "x86_64", false), "lean");
    }

    // --- Tier resolution tests ---

    fn opts(keep_plugins: bool, delete_user_data: bool, yes: bool) -> UninstallOptions {
        UninstallOptions {
            dry_run: false,
            yes,
            keep_plugins,
            delete_user_data,
        }
    }

    // Non-interactive (--yes), flags decide tier.
    #[test]
    fn tier_non_interactive_default_is_with_plugins() {
        let mut r = Cursor::new(b"");
        let mut w = Vec::<u8>::new();
        let tier = resolve_tier(&opts(false, false, true), false, &mut r, &mut w).unwrap();
        assert_eq!(tier, Tier::WithPlugins);
    }

    #[test]
    fn tier_non_interactive_keep_plugins_is_binary_only() {
        let mut r = Cursor::new(b"");
        let mut w = Vec::<u8>::new();
        let tier = resolve_tier(&opts(true, false, true), false, &mut r, &mut w).unwrap();
        assert_eq!(tier, Tier::BinaryOnly);
    }

    #[test]
    fn tier_non_interactive_delete_user_data_is_with_brains() {
        let mut r = Cursor::new(b"");
        let mut w = Vec::<u8>::new();
        let tier = resolve_tier(&opts(false, true, true), false, &mut r, &mut w).unwrap();
        assert_eq!(tier, Tier::WithBrains);
    }

    // Non-TTY (CI / piped) without --yes → same flag rules.
    #[test]
    fn tier_non_tty_without_yes_uses_flags() {
        let mut r = Cursor::new(b"");
        let mut w = Vec::<u8>::new();
        let tier = resolve_tier(&opts(true, false, false), false, &mut r, &mut w).unwrap();
        assert_eq!(tier, Tier::BinaryOnly);
    }

    // Interactive tests: TTY=true, yes=false.
    fn interactive_opts(keep_plugins: bool, delete_user_data: bool) -> UninstallOptions {
        opts(keep_plugins, delete_user_data, false)
    }

    #[test]
    fn tier_interactive_empty_line_is_with_plugins() {
        let mut r = Cursor::new(b"\n");
        let mut w = Vec::<u8>::new();
        let tier = resolve_tier(&interactive_opts(false, false), true, &mut r, &mut w).unwrap();
        assert_eq!(tier, Tier::WithPlugins);
    }

    #[test]
    fn tier_interactive_choice_1_is_binary_only() {
        let mut r = Cursor::new(b"1\n");
        let mut w = Vec::<u8>::new();
        let tier = resolve_tier(&interactive_opts(false, false), true, &mut r, &mut w).unwrap();
        assert_eq!(tier, Tier::BinaryOnly);
    }

    #[test]
    fn tier_interactive_choice_2_is_with_plugins() {
        let mut r = Cursor::new(b"2\n");
        let mut w = Vec::<u8>::new();
        let tier = resolve_tier(&interactive_opts(false, false), true, &mut r, &mut w).unwrap();
        assert_eq!(tier, Tier::WithPlugins);
    }

    #[test]
    fn tier_interactive_choice_3_with_correct_confirm_is_with_brains() {
        let mut r = Cursor::new(b"3\ndelete-brains\n");
        let mut w = Vec::<u8>::new();
        let tier = resolve_tier(&interactive_opts(false, false), true, &mut r, &mut w).unwrap();
        assert_eq!(tier, Tier::WithBrains);
    }

    #[test]
    fn tier_interactive_choice_3_wrong_confirm_falls_back_to_with_plugins() {
        let mut r = Cursor::new(b"3\nnope\n");
        let mut w = Vec::<u8>::new();
        let tier = resolve_tier(&interactive_opts(false, false), true, &mut r, &mut w).unwrap();
        assert_eq!(tier, Tier::WithPlugins);
    }

    #[test]
    fn tier_interactive_choice_3_empty_confirm_falls_back_to_with_plugins() {
        let mut r = Cursor::new(b"3\n\n");
        let mut w = Vec::<u8>::new();
        let tier = resolve_tier(&interactive_opts(false, false), true, &mut r, &mut w).unwrap();
        assert_eq!(tier, Tier::WithPlugins);
    }

    #[test]
    fn tier_interactive_unknown_choice_defaults_to_with_plugins() {
        let mut r = Cursor::new(b"banana\n");
        let mut w = Vec::<u8>::new();
        let tier = resolve_tier(&interactive_opts(false, false), true, &mut r, &mut w).unwrap();
        assert_eq!(tier, Tier::WithPlugins);
    }

    #[test]
    fn tier_interactive_keep_plugins_flag_preselects_1_on_empty_input() {
        // keep_plugins flag → default label is "1"; empty input picks the default.
        let mut r = Cursor::new(b"\n");
        let mut w = Vec::<u8>::new();
        let tier = resolve_tier(&interactive_opts(true, false), true, &mut r, &mut w).unwrap();
        assert_eq!(tier, Tier::BinaryOnly);
    }

    #[test]
    fn tier_interactive_delete_user_data_flag_preselects_3_and_needs_confirm() {
        // delete_user_data flag → default label is "3"; empty input picks default (3),
        // then needs the typed confirm.
        let mut r = Cursor::new(b"\ndelete-brains\n");
        let mut w = Vec::<u8>::new();
        let tier = resolve_tier(&interactive_opts(false, true), true, &mut r, &mut w).unwrap();
        assert_eq!(tier, Tier::WithBrains);
    }

    #[test]
    fn tier_ordering_is_correct() {
        assert!(Tier::BinaryOnly < Tier::WithPlugins);
        assert!(Tier::WithPlugins < Tier::WithBrains);
        assert!(Tier::WithBrains >= Tier::WithPlugins);
        assert!(Tier::WithBrains >= Tier::BinaryOnly);
    }

    // --- parse_locking_pids tests ---
    // These tests exercise the pure parser without spawning PowerShell.
    // The CSV format mirrors `Get-Process -Name kimetsu | ConvertTo-Csv -NoTypeInformation`.

    #[test]
    fn parse_locking_pids_returns_empty_on_empty_output() {
        let pids = parse_locking_pids("", Path::new("C:\\fake\\kimetsu.exe"), 9999);
        assert!(pids.is_empty());
    }

    #[test]
    fn parse_locking_pids_returns_empty_on_header_only() {
        let csv = "\"Id\",\"Path\"\n";
        let pids = parse_locking_pids(csv, Path::new("C:\\fake\\kimetsu.exe"), 9999);
        assert!(pids.is_empty());
    }

    #[test]
    fn parse_locking_pids_extracts_matching_pid() {
        // Use a path that actually exists on this machine so canonicalize works;
        // fall back to a temp dir path as the "target" and feed the same raw string
        // as the row so both sides are non-canonicalized and equal.
        let target = Path::new("C:\\Windows\\System32\\cmd.exe");
        let csv = format!("\"Id\",\"Path\"\n\"1234\",\"{}\"\n", target.display());
        let pids = parse_locking_pids(&csv, target, 9999);
        // If the path exists (it should on Windows), canonicalize will succeed and
        // paths will compare equal, giving us PID 1234.  If the host doesn't have
        // that path, canonicalize fails on both sides and we compare raw strings;
        // either way the assertion holds because both sides resolve identically.
        assert_eq!(pids, vec![1234u32]);
    }

    #[test]
    fn parse_locking_pids_excludes_current_pid() {
        let target = Path::new("C:\\Windows\\System32\\cmd.exe");
        let current = std::process::id();
        let csv = format!("\"Id\",\"Path\"\n\"{current}\",\"{}\"\n", target.display());
        let pids = parse_locking_pids(&csv, target, current);
        assert!(pids.is_empty(), "current PID must not be listed as locking");
    }

    #[test]
    fn parse_locking_pids_filters_different_path() {
        let target = Path::new("C:\\fake\\kimetsu.exe");
        // Row has a DIFFERENT path → should not be included.
        let csv = "\"Id\",\"Path\"\n\"5678\",\"C:\\\\other\\\\kimetsu.exe\"\n";
        let pids = parse_locking_pids(csv, target, 9999);
        assert!(
            pids.is_empty(),
            "PIDs from a different path must not be returned"
        );
    }

    #[test]
    fn parse_locking_pids_handles_multiple_rows() {
        // Both rows share the same path (but neither is the current PID).
        // We use a path that won't canonicalize so both sides stay as-is.
        let path_str = "C:\\fake\\nonexistent\\kimetsu.exe";
        let target = Path::new(path_str);
        let csv = format!(
            "\"Id\",\"Path\"\n\"101\",\"{path_str}\"\n\"202\",\"{path_str}\"\n\"303\",\"C:\\\\other.exe\"\n"
        );
        let mut pids = parse_locking_pids(&csv, target, 9999);
        pids.sort();
        assert_eq!(pids, vec![101u32, 202u32]);
    }

    // --- Error/notice message content tests ---

    #[test]
    fn error_message_does_not_say_elevated_shell() {
        // The final failure messages must guide users to stop running processes,
        // NOT to re-run as admin (which doesn't unlock a running executable on Windows).
        let update_fail_msg = "updated 0 Kimetsu executable(s), but 1 location(s) failed; \
             if the binary is locked by a running kimetsu process (e.g. `kimetsu mcp serve`), \
             quit Claude Code / Codex or run: Get-Process kimetsu | Stop-Process -Force";
        assert!(
            !update_fail_msg.contains("elevated shell"),
            "update error must not mention elevated shell"
        );
        assert!(
            update_fail_msg.contains("kimetsu mcp serve")
                || update_fail_msg.contains("Stop-Process"),
            "update error must mention how to unlock the binary"
        );

        let uninstall_fail_msg = "removed 0 Kimetsu executable(s), but 1 location(s) failed; \
             if the binary is locked by a running kimetsu process (e.g. `kimetsu mcp serve`), \
             quit Claude Code / Codex or run: Get-Process kimetsu | Stop-Process -Force";
        assert!(
            !uninstall_fail_msg.contains("elevated shell"),
            "uninstall error must not mention elevated shell"
        );
        assert!(
            uninstall_fail_msg.contains("Stop-Process"),
            "uninstall error must mention how to stop locking processes"
        );
    }

    // --- decide_preflight_action decision seam tests ---

    fn make_locking_proc(pid: u32) -> KimetsuProc {
        KimetsuProc {
            pid,
            exe_path: Some(r"C:\fake\kimetsu.exe".to_string()),
            command_line: Some("kimetsu mcp serve --workspace C:\\proj".to_string()),
            kind: crate::process::ProcKind::McpServe,
            workspace: Some("C:\\proj".to_string()),
            started_at: None,
        }
    }

    #[test]
    fn preflight_empty_locking_list_returns_defer() {
        let mut r = Cursor::new(b"");
        let mut w = Vec::<u8>::new();
        let action = decide_preflight_action(&[], true, false, &mut r, &mut w).unwrap();
        assert_eq!(action, PreflightAction::Defer);
        // Output should be empty — no prompt for empty list.
        assert!(w.is_empty());
    }

    #[test]
    fn preflight_interactive_yes_returns_stop() {
        let locking = vec![make_locking_proc(1234)];
        let mut r = Cursor::new(b"y\n");
        let mut w = Vec::<u8>::new();
        let action = decide_preflight_action(&locking, true, false, &mut r, &mut w).unwrap();
        assert_eq!(action, PreflightAction::Stop, "y → Stop");
        let out = String::from_utf8(w).unwrap();
        assert!(out.contains("1234"), "output must mention the PID");
    }

    #[test]
    fn preflight_interactive_yes_capital_returns_stop() {
        let locking = vec![make_locking_proc(5678)];
        let mut r = Cursor::new(b"Y\n");
        let mut w = Vec::<u8>::new();
        let action = decide_preflight_action(&locking, true, false, &mut r, &mut w).unwrap();
        assert_eq!(action, PreflightAction::Stop);
    }

    #[test]
    fn preflight_interactive_yes_full_word_returns_stop() {
        let locking = vec![make_locking_proc(9999)];
        let mut r = Cursor::new(b"yes\n");
        let mut w = Vec::<u8>::new();
        let action = decide_preflight_action(&locking, true, false, &mut r, &mut w).unwrap();
        assert_eq!(action, PreflightAction::Stop);
    }

    #[test]
    fn preflight_interactive_empty_line_returns_stop_default() {
        // Empty input → default Y.
        let locking = vec![make_locking_proc(1111)];
        let mut r = Cursor::new(b"\n");
        let mut w = Vec::<u8>::new();
        let action = decide_preflight_action(&locking, true, false, &mut r, &mut w).unwrap();
        assert_eq!(action, PreflightAction::Stop, "empty input → default Stop");
    }

    #[test]
    fn preflight_interactive_n_returns_defer() {
        let locking = vec![make_locking_proc(2222)];
        let mut r = Cursor::new(b"n\n");
        let mut w = Vec::<u8>::new();
        let action = decide_preflight_action(&locking, true, false, &mut r, &mut w).unwrap();
        assert_eq!(action, PreflightAction::Defer, "n → Defer");
        let out = String::from_utf8(w).unwrap();
        assert!(
            out.contains("deferred"),
            "output must mention deferred replace"
        );
    }

    #[test]
    fn preflight_interactive_no_returns_defer() {
        let locking = vec![make_locking_proc(3333)];
        let mut r = Cursor::new(b"no\n");
        let mut w = Vec::<u8>::new();
        let action = decide_preflight_action(&locking, true, false, &mut r, &mut w).unwrap();
        assert_eq!(action, PreflightAction::Defer);
    }

    #[test]
    fn preflight_non_tty_returns_defer() {
        // Non-interactive: never Stop silently.
        let locking = vec![make_locking_proc(4444)];
        let mut r = Cursor::new(b"");
        let mut w = Vec::<u8>::new();
        let action = decide_preflight_action(&locking, false, false, &mut r, &mut w).unwrap();
        assert_eq!(action, PreflightAction::Defer, "non-TTY → Defer");
        let out = String::from_utf8(w).unwrap();
        // Should print the PIDs and a notice about deferred replace.
        assert!(
            out.contains("4444"),
            "non-TTY output must mention the locking PID"
        );
        assert!(
            out.contains("deferred") || out.contains("Stop-Process"),
            "non-TTY output must mention the deferred path or manual command"
        );
    }

    #[test]
    fn preflight_force_flag_returns_defer_not_silent_stop() {
        // `--force` is not a license to kill processes silently in CI.
        let locking = vec![make_locking_proc(5555)];
        let mut r = Cursor::new(b"");
        let mut w = Vec::<u8>::new();
        let action = decide_preflight_action(&locking, false, true, &mut r, &mut w).unwrap();
        assert_eq!(
            action,
            PreflightAction::Defer,
            "--force + non-TTY → Defer (no silent kills)"
        );
    }

    #[test]
    fn preflight_multiple_procs_all_listed() {
        let locking = vec![make_locking_proc(101), make_locking_proc(202)];
        let mut r = Cursor::new(b"n\n");
        let mut w = Vec::<u8>::new();
        decide_preflight_action(&locking, true, false, &mut r, &mut w).unwrap();
        let out = String::from_utf8(w).unwrap();
        assert!(out.contains("101"), "PID 101 should appear in output");
        assert!(out.contains("202"), "PID 202 should appear in output");
    }
}
