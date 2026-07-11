use crate::{
    config::{Config, DEFAULT_PORT},
    server,
};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    time::{SystemTime, UNIX_EPOCH},
};

const SOL: &str = "codex-sol";
const TERRA: &str = "codex-terra";
const LUNA: &str = "codex-luna";
const BEGIN: &str = "# BEGIN codex-for-grok owned model: ";
const END: &str = "# END codex-for-grok owned model: ";
const OWNER: &str = "JoaquinCampo/codex-for-grok";
const LABEL: &str = "com.joaquincampo.codex-for-grok";

#[derive(Parser)]
#[command(version, about, arg_required_else_help = false)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Action>,
}
#[derive(Subcommand)]
enum Action {
    Run,
    Start,
    Stop,
    Restart,
    Status,
    Doctor,
    Setup {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    Uninstall {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        config: Option<PathBuf>,
    },
}
#[derive(Serialize, Deserialize, Default, Clone)]
struct Manifest {
    version: u8,
    config: PathBuf,
    blocks: Vec<OwnedBlock>,
    #[serde(default)]
    service: Option<OwnedFile>,
}
#[derive(Serialize, Deserialize, Clone)]
struct OwnedBlock {
    name: String,
    bytes: String,
    digest: String,
}
#[derive(Serialize, Deserialize, Clone)]
struct OwnedFile {
    path: PathBuf,
    digest: String,
}

pub async fn entry() -> ExitCode {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Action::Run) {
        Action::Run => run().await,
        Action::Start => out(install_and_start()),
        Action::Stop => out(stop_service()),
        Action::Restart => out(restart_service()),
        Action::Status => status().await,
        Action::Doctor => doctor().await,
        Action::Setup { dry_run, config } => out(setup(config, dry_run)),
        Action::Uninstall { dry_run, config } => out(uninstall(config, dry_run)),
    }
}
fn out(r: Result<String, String>) -> ExitCode {
    match r {
        Ok(s) => {
            println!("{s}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
async fn run() -> ExitCode {
    let c = match Config::from_env() {
        Ok(v) => v,
        Err(e) => return out(Err(e)),
    };
    let lock = match acquire_lock() {
        Ok(v) => v,
        Err(e) => return out(Err(e)),
    };
    let state = match server::AppState::new(c) {
        Ok(v) => v,
        Err(e) => return out(Err(e.to_string())),
    };
    let metrics = state.metrics();
    let quota = state.quota();
    let shutdown = async move {
        wait_signal().await;
        metrics.set_ready(false)
    };
    let r = server::serve(state, shutdown).await;
    quota.shutdown().await;
    drop(lock);
    out(r
        .map(|_| "bridge stopped".into())
        .map_err(|e| e.to_string()))
}
async fn wait_signal() {
    let mut t =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
    let mut i =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).expect("signal");
    tokio::select! {_=t.recv()=>{},_=i.recv()=>{}}
}
fn home() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is not set".into())
}
fn state_dir() -> Result<PathBuf, String> {
    Ok(home()?.join(".grok/codex-for-grok"))
}
fn manifest_path() -> Result<PathBuf, String> {
    Ok(state_dir()?.join("ownership.json"))
}
fn config_path(p: Option<PathBuf>) -> Result<PathBuf, String> {
    Ok(
        p.or_else(|| env::var_os("GROK_CONFIG_PATH").map(Into::into))
            .unwrap_or(home()?.join(".grok/config.toml")),
    )
}
fn digest(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
fn block(name: &str, model: &str) -> String {
    format!(
        "{BEGIN}{name}\n[models.{name}]\nname = \"{name}\"\nmodel = \"{model}\"\nbase_url = \"http://127.0.0.1:{DEFAULT_PORT}/v1\"\napi_key = \"local\"\n{END}{name}\n"
    )
}
fn expected(name: &str) -> (&'static str, String) {
    (
        if name == SOL {
            "gpt-5.6-sol"
        } else {
            "gpt-5.6-terra"
        },
        format!("http://127.0.0.1:{DEFAULT_PORT}/v1"),
    )
}
fn validate_model(name: &str, v: &toml::Value) -> Result<(), String> {
    let (model, url) = expected(name);
    let checks = [
        ("name", name),
        ("model", model),
        ("base_url", url.as_str()),
        ("api_key", "local"),
    ];
    for (field, want) in checks {
        if v.get(field).and_then(toml::Value::as_str) != Some(want) {
            return Err(format!(
                "conflicting [models.{name}]: expected {field} = {want:?}"
            ));
        }
    }
    Ok(())
}
fn load_manifest(mp: &Path) -> Result<Option<Manifest>, String> {
    match fs::read(mp) {
        Ok(b) => serde_json::from_slice(&b)
            .map(Some)
            .map_err(|e| format!("invalid ownership manifest: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}
fn verify_manifest(m: &Manifest, path: &Path, bytes: &[u8]) -> Result<(), String> {
    if m.version != 1 || m.config != path {
        return Err("ownership manifest version/config mismatch".into());
    }
    for b in &m.blocks {
        if digest(b.bytes.as_bytes()) != b.digest {
            return Err("ownership manifest block digest mismatch".into());
        }
        if bytes
            .windows(b.bytes.len())
            .filter(|w| *w == b.bytes.as_bytes())
            .count()
            != 1
        {
            return Err(format!(
                "owned {} block changed, missing, or duplicated",
                b.name
            ));
        }
    }
    Ok(())
}
fn setup(given: Option<PathBuf>, dry: bool) -> Result<String, String> {
    require_prerequisite(
        "grok",
        "install Grok Build from the official xAI distribution",
    )?;
    require_prerequisite("codex", "install the Codex CLI and run `codex login`")?;
    let path = config_path(given)?;
    setup_at(path, manifest_path()?, dry)
}
fn executable_on_path(name: &str) -> bool {
    env::var_os("PATH")
        .is_some_and(|path| env::split_paths(&path).any(|directory| directory.join(name).is_file()))
}
fn require_prerequisite(name: &str, guidance: &str) -> Result<(), String> {
    if executable_on_path(name) {
        Ok(())
    } else {
        Err(format!(
            "required `{name}` executable not found on PATH; {guidance}"
        ))
    }
}
fn setup_at(path: PathBuf, mp: PathBuf, dry: bool) -> Result<String, String> {
    let old = match fs::read(&path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(e) => return Err(e.to_string()),
    };
    let text = std::str::from_utf8(&old).map_err(|_| "config is not UTF-8; refusing to modify")?;
    let doc: toml::Value = if old.is_empty() {
        toml::Value::Table(Default::default())
    } else {
        toml::from_str(text).map_err(|e| format!("invalid TOML: {e}"))?
    };
    let models = doc.get("models").and_then(toml::Value::as_table);
    if models.and_then(|m| m.get(LUNA)).is_some() {
        return Err("Luna is explicitly unsupported".into());
    }
    let mut manifest = load_manifest(&mp)?.unwrap_or(Manifest {
        version: 1,
        config: path.clone(),
        blocks: vec![],
        service: None,
    });
    if manifest.version != 1 || manifest.config != path {
        return Err("ownership manifest version/config mismatch".into());
    }
    for owned in &manifest.blocks {
        if digest(owned.bytes.as_bytes()) != owned.digest {
            return Err("ownership manifest block digest mismatch".into());
        }
    }
    let mut relinquished = 0;
    manifest.blocks.retain(|owned| {
        let exact = old
            .windows(owned.bytes.len())
            .filter(|window| *window == owned.bytes.as_bytes())
            .count();
        let semantically_intact = exact == 0
            && models
                .and_then(|entries| entries.get(&owned.name))
                .is_some_and(|value| validate_model(&owned.name, value).is_ok());
        if semantically_intact {
            relinquished += 1;
            false
        } else {
            true
        }
    });
    verify_manifest(&manifest, &path, &old)?;
    let mut add = vec![];
    for (name, model) in [(SOL, "gpt-5.6-sol"), (TERRA, "gpt-5.6-terra")] {
        if let Some(v) = models.and_then(|m| m.get(name)) {
            validate_model(name, v)?
        } else {
            let bytes = block(name, model);
            add.push(OwnedBlock {
                name: name.into(),
                digest: digest(bytes.as_bytes()),
                bytes,
            })
        }
    }
    if add.is_empty() {
        if relinquished == 0 {
            return Ok("already configured; ownership verified".into());
        }
        if dry {
            return Ok(format!(
                "dry-run: would preserve and relinquish ownership of {relinquished} semantically intact model block(s)"
            ));
        }
        write_manifest(&mp, &manifest)?;
        return Ok(format!(
            "configuration reconciled; preserved {relinquished} model block(s) as user-owned"
        ));
    }
    if dry {
        return Ok(format!(
            "dry-run: would append {} model block(s) and relinquish ownership of {relinquished} existing block(s)",
            add.len()
        ));
    }
    let mut next = old.clone();
    if !next.is_empty() && !next.ends_with(b"\n") {
        next.push(b'\n')
    }
    if !next.is_empty() {
        next.push(b'\n')
    }
    for b in &add {
        next.extend_from_slice(b.bytes.as_bytes());
        next.push(b'\n')
    }
    backup_and_write(&path, &old, &next)?;
    manifest.blocks.extend(add);
    if let Err(error) = write_manifest(&mp, &manifest) {
        atomic_write(&path, &old).map_err(|rollback| {
            format!("manifest write failed ({error}); config rollback also failed ({rollback})")
        })?;
        return Err(format!(
            "manifest write failed; configuration was rolled back: {error}"
        ));
    }
    Ok(format!("configured {}", path.display()))
}
fn write_manifest(path: &Path, m: &Manifest) -> Result<(), String> {
    atomic_write(
        path,
        &serde_json::to_vec_pretty(m).map_err(|e| e.to_string())?,
    )
}
fn uninstall(given: Option<PathBuf>, dry: bool) -> Result<String, String> {
    let mp = manifest_path()?;
    let m = load_manifest(&mp)?.ok_or("no ownership manifest; refusing uninstall")?;
    let path = config_path(given)?;
    let old = fs::read(&path).map_err(|e| e.to_string())?;
    verify_manifest(&m, &path, &old)?;
    if let Some(s) = &m.service {
        verify_owned_file(s)?
    }
    if dry {
        return Ok(
            "dry-run: ownership verified; would stop service and remove owned artifacts".into(),
        );
    }
    let previous_manager = if m.service.is_some() {
        Some(manager_state()?)
    } else {
        None
    };
    stop_disable_if_installed(&m)?;
    let mut next = old.clone();
    for b in &m.blocks {
        let at = next
            .windows(b.bytes.len())
            .position(|w| w == b.bytes.as_bytes())
            .ok_or("owned block disappeared")?;
        next.drain(at..at + b.bytes.len());
    }
    let service_backup = m
        .service
        .as_ref()
        .map(|s| fs::read(&s.path).map(|bytes| (s.path.clone(), bytes)))
        .transpose()
        .map_err(|e| e.to_string())?;
    let transaction = (|| -> Result<(), String> {
        backup_and_write(&path, &old, &next)?;
        if let Some(s) = &m.service {
            fs::remove_file(&s.path).map_err(|e| e.to_string())?;
            reload_service_manager()?;
        }
        fs::remove_file(&mp).map_err(|e| e.to_string())?;
        Ok(())
    })();
    if let Err(error) = transaction {
        let mut rollback_errors = Vec::new();
        if let Err(e) = atomic_write(&path, &old) {
            rollback_errors.push(format!("config: {e}"));
        }
        if let Some((service_path, bytes)) = service_backup {
            if let Err(e) = atomic_write(&service_path, &bytes) {
                rollback_errors.push(format!("service: {e}"));
            } else if let Some(state) = previous_manager
                && let Err(e) = restore_manager_state(state, &service_path)
            {
                rollback_errors.push(format!("manager state: {e}"));
            }
        }
        if rollback_errors.is_empty() {
            return Err(format!(
                "uninstall failed and artifacts were restored; retry is safe: {error}"
            ));
        }
        return Err(format!(
            "uninstall failed ({error}); rollback incomplete: {}",
            rollback_errors.join(", ")
        ));
    }
    Ok("uninstalled unchanged bridge-owned configuration and service".into())
}
fn backup_and_write(path: &Path, old: &[u8], new: &[u8]) -> Result<(), String> {
    if path.exists() {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        fs::write(path.with_extension(format!("backup-{n}")), old).map_err(|e| e.to_string())?
    }
    atomic_write(path, new)
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).map_err(|e| e.to_string())?
    }
    let tmp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos()
    ));
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    f.write_all(bytes)
        .and_then(|_| f.sync_all())
        .map_err(|e| e.to_string())?;
    fs::rename(tmp, path).map_err(|e| e.to_string())
}
fn acquire_lock() -> Result<fs::File, String> {
    use fs2::FileExt;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let d = state_dir()?.join("run");
    fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    let f = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(d.join("codex-for-grok.lock"))
        .map_err(|e| e.to_string())?;
    f.try_lock_exclusive()
        .map_err(|_| "another bridge process owns the lock".into())
        .map(|_| f)
}
fn executable() -> Result<PathBuf, String> {
    env::current_exe().map_err(|e| e.to_string())
}
fn xml_string(value: &Path) -> Result<String, String> {
    let value = value
        .to_str()
        .ok_or("service executable path is not valid UTF-8 and cannot be represented in a plist")?;
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}
fn systemd_exec_arg(value: &Path) -> Result<String, String> {
    let value = value.to_str().ok_or(
        "service executable path is not valid UTF-8 and cannot be represented in a systemd unit",
    )?;
    Ok(format!(
        "\"{}\"",
        value
            .replace('%', "%%")
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    ))
}
fn service_definition_for(os: &str, root: &Path, x: &Path) -> Result<(PathBuf, Vec<u8>), String> {
    if os == "macos" {
        let p = root.join(format!("Library/LaunchAgents/{LABEL}.plist"));
        let b = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!-- owner: {OWNER} -->\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{LABEL}</string><key>ProgramArguments</key><array><string>{}</string><string>run</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><true/></dict></plist>\n",
            xml_string(x)?
        );
        Ok((p, b.into_bytes()))
    } else if os == "linux" {
        let p = root.join(".config/systemd/user/codex-for-grok.service");
        let b = format!(
            "# owner: {OWNER}\n[Unit]\nDescription=Codex for Grok\n[Service]\nExecStart={} run\nRestart=on-failure\n[Install]\nWantedBy=default.target\n",
            systemd_exec_arg(x)?
        );
        Ok((p, b.into_bytes()))
    } else {
        Err("services supported only on macOS and Linux".into())
    }
}
fn service_definition() -> Result<(PathBuf, Vec<u8>), String> {
    service_definition_for(env::consts::OS, &home()?, &executable()?)
}
fn verify_owned_file(s: &OwnedFile) -> Result<(), String> {
    let b = fs::read(&s.path)
        .map_err(|_| format!("owned service definition missing: {}", s.path.display()))?;
    if digest(&b) != s.digest {
        return Err(format!(
            "service definition conflict: {} is not unchanged bridge-owned content",
            s.path.display()
        ));
    }
    Ok(())
}
fn commit_service_file(
    path: &Path,
    body: &[u8],
    mp: &Path,
    m: &mut Manifest,
) -> Result<(), String> {
    atomic_write(path, body)?;
    m.service = Some(OwnedFile {
        path: path.to_path_buf(),
        digest: digest(body),
    });
    if let Err(error) = write_manifest(mp, m) {
        fs::remove_file(path).map_err(|rollback| {
            format!("manifest write failed ({error}); service rollback failed ({rollback})")
        })?;
        m.service = None;
        return Err(format!(
            "manifest write failed; service file was rolled back: {error}"
        ));
    }
    Ok(())
}
fn install_and_start() -> Result<String, String> {
    let mp = manifest_path()?;
    let mut m = load_manifest(&mp)?.ok_or("run setup before start")?;
    let (path, body) = service_definition()?;
    if let Some(s) = &m.service {
        verify_owned_file(s)?;
        if s.path != path || s.digest != digest(&body) {
            return Err("installed service belongs to another binary; uninstall it first".into());
        }
    } else if path.exists() {
        return Err(format!(
            "service definition already exists and is not owned: {}",
            path.display()
        ));
    } else {
        commit_service_file(&path, &body, &mp, &mut m)?;
    }
    start_manager(&path)?;
    Ok("service installed and started".into())
}
fn restart_service() -> Result<String, String> {
    let m =
        load_manifest(&manifest_path()?)?.ok_or("service is not installed; run setup and start")?;
    let s = m
        .service
        .as_ref()
        .ok_or("service is not installed; run start")?;
    verify_owned_file(s)?;
    manager_restart()?;
    Ok("installed service restarted".into())
}
fn stop_service() -> Result<String, String> {
    let m = load_manifest(&manifest_path()?)?.ok_or("service is not installed")?;
    let s = m.service.as_ref().ok_or("service is not installed")?;
    verify_owned_file(s)?;
    manager_stop()?;
    Ok("installed service stopped".into())
}
fn uid() -> Result<u32, String> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(home()?)
        .map(|m| m.uid())
        .map_err(|e| format!("cannot determine user UID: {e}"))
}
fn start_manager(path: &Path) -> Result<(), String> {
    if cfg!(target_os = "macos") {
        let domain = format!("gui/{}", uid()?);
        let target = format!("{domain}/{LABEL}");
        if command_success("launchctl", &["print", &target])? {
            return Ok(());
        }
        cmd(
            "launchctl",
            &[
                "bootstrap",
                &domain,
                path.to_str().ok_or("non-UTF-8 service path")?,
            ],
        )
    } else {
        cmd("systemctl", &["--user", "daemon-reload"])?;
        cmd(
            "systemctl",
            &["--user", "enable", "--now", "codex-for-grok.service"],
        )
    }
}
fn manager_restart() -> Result<(), String> {
    if cfg!(target_os = "macos") {
        cmd(
            "launchctl",
            &["kickstart", "-k", &format!("gui/{}/{}", uid()?, LABEL)],
        )
    } else {
        cmd(
            "systemctl",
            &["--user", "restart", "codex-for-grok.service"],
        )
    }
}
fn manager_stop() -> Result<(), String> {
    if cfg!(target_os = "macos") {
        let target = format!("gui/{}/{}", uid()?, LABEL);
        if !command_success("launchctl", &["print", &target])? {
            return Ok(());
        }
        cmd("launchctl", &["bootout", &target])
    } else {
        if !systemctl_query(
            &["--user", "is-active", "--quiet", "codex-for-grok.service"],
            &[3, 4],
        )? {
            return Ok(());
        }
        cmd("systemctl", &["--user", "stop", "codex-for-grok.service"])
    }
}
#[derive(Clone, Copy)]
struct ManagerState {
    active: bool,
    enabled: bool,
}
fn manager_state() -> Result<ManagerState, String> {
    if cfg!(target_os = "macos") {
        Ok(ManagerState {
            active: manager_is_active()?,
            enabled: false,
        })
    } else {
        Ok(ManagerState {
            active: manager_is_active()?,
            enabled: systemctl_query(
                &["--user", "is-enabled", "--quiet", "codex-for-grok.service"],
                &[1],
            )?,
        })
    }
}
fn restore_manager_state(state: ManagerState, path: &Path) -> Result<(), String> {
    if cfg!(target_os = "macos") {
        if state.active {
            start_manager(path)
        } else {
            Ok(())
        }
    } else {
        reload_service_manager()?;
        if state.enabled {
            cmd("systemctl", &["--user", "enable", "codex-for-grok.service"])?;
        }
        if state.active {
            cmd("systemctl", &["--user", "start", "codex-for-grok.service"])?;
        }
        Ok(())
    }
}
fn stop_disable_if_installed(m: &Manifest) -> Result<(), String> {
    if m.service.is_none() {
        return Ok(());
    }
    if cfg!(target_os = "macos") {
        manager_stop()
    } else {
        manager_stop()?;
        if systemctl_query(
            &["--user", "is-enabled", "--quiet", "codex-for-grok.service"],
            &[1],
        )? {
            cmd(
                "systemctl",
                &["--user", "disable", "codex-for-grok.service"],
            )?;
        }
        Ok(())
    }
}
fn reload_service_manager() -> Result<(), String> {
    if cfg!(target_os = "linux") {
        cmd("systemctl", &["--user", "daemon-reload"])
    } else {
        Ok(())
    }
}
fn manager_is_active() -> Result<bool, String> {
    if cfg!(target_os = "macos") {
        command_success(
            "launchctl",
            &["print", &format!("gui/{}/{}", uid()?, LABEL)],
        )
    } else if cfg!(target_os = "linux") {
        systemctl_query(
            &["--user", "is-active", "--quiet", "codex-for-grok.service"],
            &[3, 4],
        )
    } else {
        Err("services supported only on macOS and Linux".into())
    }
}
fn cmd(p: &str, a: &[&str]) -> Result<(), String> {
    let s = Command::new(p)
        .args(a)
        .status()
        .map_err(|e| format!("could not run {p}: {e}"))?;
    if s.success() {
        Ok(())
    } else {
        Err(format!("{p} exited with {s}"))
    }
}
fn systemctl_query(args: &[&str], benign_false_codes: &[i32]) -> Result<bool, String> {
    let status = Command::new("systemctl")
        .args(args)
        .status()
        .map_err(|e| format!("could not run systemctl: {e}"))?;
    if status.success() {
        return Ok(true);
    }
    if status
        .code()
        .is_some_and(|code| benign_false_codes.contains(&code))
    {
        return Ok(false);
    }
    Err(format!("systemctl query exited with {status}"))
}
fn command_success(p: &str, a: &[&str]) -> Result<bool, String> {
    Command::new(p)
        .args(a)
        .output()
        .map(|output| output.status.success())
        .map_err(|e| format!("could not run {p}: {e}"))
}
async fn fetch_identity(path: &str) -> Result<Value, String> {
    let url = format!("http://127.0.0.1:{DEFAULT_PORT}{path}");
    let r = reqwest::Client::new()
        .get(url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .map_err(|e| format!("bridge unavailable: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("bridge returned HTTP {}", r.status()));
    }
    let v: Value = r
        .json()
        .await
        .map_err(|e| format!("invalid bridge JSON: {e}"))?;
    if v.get("service").and_then(Value::as_str) != Some("codex-for-grok") {
        return Err("listener is not codex-for-grok".into());
    }
    Ok(v)
}
async fn status() -> ExitCode {
    out(fetch_identity("/status").await.map(|v| v.to_string()))
}
async fn doctor() -> ExitCode {
    let mut failures = vec![];
    if !executable_on_path("grok") {
        failures.push("Grok Build executable `grok` is not available on PATH".into());
    }
    if !executable_on_path("codex") {
        failures.push("Codex CLI executable `codex` is not available on PATH".into());
    }
    let config = config_path(None);
    match (&config, load_manifest(&manifest_path().unwrap_or_default())) {
        (Ok(p), Ok(Some(m))) => match fs::read(p) {
            Ok(b) => {
                if let Err(e) = verify_manifest(&m, p, &b) {
                    failures.push(e)
                }
                if let Some(s) = &m.service {
                    if let Err(e) = verify_owned_file(s) {
                        failures.push(e)
                    } else {
                        match manager_is_active() {
                            Ok(true) => {}
                            Ok(false) => failures.push("owned service is not loaded/active".into()),
                            Err(e) => failures.push(format!("service manager check failed: {e}")),
                        }
                    }
                }
            }
            Err(e) => failures.push(format!("config unreadable: {e}")),
        },
        (Err(e), _) => failures.push(e.clone()),
        (_, Ok(None)) => failures.push("ownership manifest missing; run setup".into()),
        (_, Err(e)) => failures.push(e),
    }
    if let Ok(c) = Config::from_env() {
        if !c.auth_path.is_file() {
            failures.push("Codex auth missing; run `codex login`".into())
        }
    } else {
        failures.push("invalid bridge environment".into())
    }
    if let Err(e) = fetch_identity("/healthz").await {
        failures.push(e)
    }
    if failures.is_empty() {
        out(Ok("doctor: all checks passed".into()))
    } else {
        out(Err(failures.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    fn temp() -> PathBuf {
        let p = env::temp_dir().join(format!(
            "gcb-test-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }
    fn manifest_for(path: &Path, blocks: Vec<OwnedBlock>) -> Manifest {
        Manifest {
            version: 1,
            config: path.into(),
            blocks,
            service: None,
        }
    }
    #[test]
    fn setup_twice_preserves_config_and_dry_run_is_invariant() {
        let d = temp();
        let p = d.join("config.toml");
        let mp = d.join("ownership.json");
        let old = b"# custom\r\n[models.custom]\r\nmodel = \"mine\"\r\n";
        fs::write(&p, old).unwrap();
        setup_at(p.clone(), mp.clone(), true).unwrap();
        assert_eq!(fs::read(&p).unwrap(), old);
        assert!(!mp.exists());
        setup_at(p.clone(), mp.clone(), false).unwrap();
        let once = fs::read(&p).unwrap();
        assert!(once.starts_with(old));
        let manifest_once = fs::read(&mp).unwrap();
        setup_at(p.clone(), mp.clone(), false).unwrap();
        assert_eq!(fs::read(&p).unwrap(), once);
        assert_eq!(fs::read(&mp).unwrap(), manifest_once);
    }

    #[test]
    fn setup_reconciles_semantically_intact_blocks_after_markers_are_stripped() {
        let d = temp();
        let p = d.join("config.toml");
        let mp = d.join("ownership.json");
        setup_at(p.clone(), mp.clone(), false).unwrap();
        let mut manifest = load_manifest(&mp).unwrap().unwrap();
        manifest.service = Some(OwnedFile {
            path: d.join("service"),
            digest: "preserved".into(),
        });
        write_manifest(&mp, &manifest).unwrap();
        let plain = b"[models.codex-sol]\nname = \"codex-sol\"\nmodel = \"gpt-5.6-sol\"\nbase_url = \"http://127.0.0.1:18474/v1\"\napi_key = \"local\"\n\n[models.codex-terra]\nname = \"codex-terra\"\nmodel = \"gpt-5.6-terra\"\nbase_url = \"http://127.0.0.1:18474/v1\"\napi_key = \"local\"\n";
        fs::write(&p, plain).unwrap();

        let dry = setup_at(p.clone(), mp.clone(), true).unwrap();
        assert!(dry.contains("relinquish ownership of 2"));
        assert_eq!(load_manifest(&mp).unwrap().unwrap().blocks.len(), 2);

        let result = setup_at(p.clone(), mp.clone(), false).unwrap();
        assert!(result.contains("preserved 2 model block(s) as user-owned"));
        assert_eq!(fs::read(&p).unwrap(), plain);
        let reconciled = load_manifest(&mp).unwrap().unwrap();
        assert!(reconciled.blocks.is_empty());
        assert_eq!(reconciled.service.unwrap().digest, "preserved");
    }

    #[test]
    fn setup_conflict_does_not_mutate_either_file() {
        let d = temp();
        let p = d.join("config.toml");
        let mp = d.join("ownership.json");
        let old =
            b"[models.codex-sol]\nname='codex-sol'\nmodel='wrong'\nbase_url='x'\napi_key='local'\n";
        fs::write(&p, old).unwrap();
        assert!(setup_at(p.clone(), mp.clone(), false).is_err());
        assert_eq!(fs::read(&p).unwrap(), old);
        assert!(!mp.exists());
    }

    #[test]
    fn setup_rolls_back_config_when_manifest_cannot_be_written() {
        let d = temp();
        let p = d.join("config.toml");
        let old = b"# untouched\n";
        fs::write(&p, old).unwrap();
        let blocker = d.join("blocker");
        fs::write(&blocker, b"not a directory").unwrap();
        assert!(setup_at(p.clone(), blocker.join("manifest.json"), false).is_err());
        assert_eq!(fs::read(&p).unwrap(), old);
    }

    #[test]
    fn generates_owned_service_definitions_for_macos_and_linux() {
        let d = temp();
        let exe = Path::new("/opt/grok bridge/bin");
        let (plist_path, plist) = service_definition_for("macos", &d, exe).unwrap();
        assert!(plist_path.ends_with(format!("Library/LaunchAgents/{LABEL}.plist")));
        assert!(String::from_utf8(plist).unwrap().contains(OWNER));
        let (unit_path, unit) = service_definition_for("linux", &d, exe).unwrap();
        assert!(unit_path.ends_with(".config/systemd/user/codex-for-grok.service"));
        let unit = String::from_utf8(unit).unwrap();
        assert!(unit.contains(OWNER));
        assert!(unit.contains("ExecStart=\"/opt/grok bridge/bin\" run"));
    }
    #[test]
    fn semantic_conflicts_check_every_field() {
        let bad:toml::Value=toml::from_str("name='codex-sol'\nmodel='gpt-5.6-sol'\nbase_url='http://127.0.0.1:18474/v1'\napi_key='wrong'").unwrap();
        assert!(validate_model(SOL, &bad).unwrap_err().contains("api_key"))
    }
    #[test]
    fn manifest_rejects_changed_or_missing_blocks() {
        let p = PathBuf::from("x");
        let b = block(SOL, "gpt-5.6-sol");
        let m = manifest_for(
            &p,
            vec![OwnedBlock {
                name: SOL.into(),
                digest: digest(b.as_bytes()),
                bytes: b.clone(),
            }],
        );
        assert!(verify_manifest(&m, &p, b"other").is_err());
        assert!(verify_manifest(&m, &p, b.as_bytes()).is_ok())
    }
    #[test]
    fn service_arguments_are_escaped_for_each_manager() {
        let d = temp();
        let exe = Path::new("/opt/a b/quo\"te\\100%");
        let (_, unit) = service_definition_for("linux", &d, exe).unwrap();
        let unit = String::from_utf8(unit).unwrap();
        assert!(unit.contains("ExecStart=\"/opt/a b/quo\\\"te\\\\100%%\" run"));
        let xml_exe = Path::new("/opt/a&b/<tool>\"");
        let (_, plist) = service_definition_for("macos", &d, xml_exe).unwrap();
        let plist = String::from_utf8(plist).unwrap();
        assert!(plist.contains("/opt/a&amp;b/&lt;tool&gt;&quot;"));
    }

    #[cfg(unix)]
    #[test]
    fn service_definition_refuses_non_utf8_paths() {
        use std::os::unix::ffi::OsStrExt;
        let d = temp();
        let exe = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/invalid-\xff"));
        assert!(service_definition_for("linux", &d, exe).is_err());
        assert!(service_definition_for("macos", &d, exe).is_err());
    }

    #[test]
    fn service_transaction_rolls_back_on_manifest_failure() {
        let d = temp();
        let service = d.join("service.unit");
        let blocker = d.join("blocker");
        fs::write(&blocker, b"file").unwrap();
        let config = d.join("config.toml");
        let mut manifest = manifest_for(&config, vec![]);
        assert!(
            commit_service_file(&service, b"owned", &blocker.join("manifest"), &mut manifest)
                .is_err()
        );
        assert!(!service.exists());
        assert!(manifest.service.is_none());
    }

    #[test]
    fn command_execution_errors_are_not_reported_as_inactive() {
        let error = command_success("/definitely/missing/grok-bridge-command", &[]).unwrap_err();
        assert!(error.contains("could not run"));
    }

    #[test]
    fn uninstall_algorithm_preserves_unrelated_bytes() {
        let owned = block(TERRA, "gpt-5.6-terra");
        let mut all = b"before\r\n".to_vec();
        all.extend_from_slice(owned.as_bytes());
        all.extend_from_slice(b"after\n");
        let at = all
            .windows(owned.len())
            .position(|w| w == owned.as_bytes())
            .unwrap();
        all.drain(at..at + owned.len());
        assert_eq!(all, b"before\r\nafter\n")
    }
}
