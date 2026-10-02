//! Developer workflow tasks for agent-observer: run/gate/compare/pack.
//!
//! Detached from the root crate (own `[workspace]` in `xtask/Cargo.toml`), so
//! the platform build never compiles this dependency tree. All paths resolve
//! from the repo root (the parent of this crate's manifest dir); the Python
//! starter kit is assumed at `tmp/agent-observer-starter-kit`.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::Value;
use sha2::{Digest, Sha256};

const KIT_DIR: &str = "tmp/agent-observer-starter-kit";

#[derive(Parser)]
#[command(name = "xtask", about = "Developer workflow tasks for agent-observer")]
struct Cli {
    #[command(subcommand)]
    task: Task,
}

#[derive(Subcommand)]
enum Task {
    /// Build what's needed, then run a scenario end-to-end into out/<scenario>-<agent>/
    Run {
        /// Scenario name under tmp/agent-observer-starter-kit/scenarios/
        scenario: String,
        #[arg(long, default_value = "baseline")]
        agent: AgentKind,
        #[arg(long)]
        release: bool,
        /// Global wall-clock budget in seconds (forwarded to `agent-observer run`)
        #[arg(long)]
        wallclock: Option<f64>,
    },
    /// Fast suite (`cargo test`) plus all end-to-end gates (`--release --ignored`)
    Gate,
    /// Run baseline, reference, and sac-agent on one scenario; print a totals table
    Compare {
        scenario: String,
        #[arg(long)]
        release: bool,
    },
    /// Pack the platform submission ZIP and verify the manifest build from it
    Pack {
        /// Output ZIP path (relative to the repo root)
        #[arg(long, default_value = "my-agent.zip")]
        out: PathBuf,
    },
    /// Serve the strategy book with live reload (mdBook: builds, opens a browser, blocks)
    Book {
        /// One-shot build into book/book/ instead of serving
        #[arg(long)]
        build: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum AgentKind {
    Baseline,
    Reference,
    SacAgent,
    Python,
}

impl AgentKind {
    fn label(&self) -> &'static str {
        match self {
            AgentKind::Baseline => "baseline",
            AgentKind::Reference => "reference",
            AgentKind::SacAgent => "sac-agent",
            AgentKind::Python => "python",
        }
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask manifest lives in <repo>/xtask")
        .to_path_buf()
}

/// Run a command with inherited stdio; exit this process with the child's
/// code on failure.
fn run_inherited(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("spawning {:?}", command.get_program()))?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

fn cargo_build(root: &Path, release: bool, bins: &[&str], quiet: bool) -> Result<()> {
    let mut command = Command::new("cargo");
    command.arg("build").current_dir(root);
    if release {
        command.arg("--release");
    }
    for bin in bins {
        command.arg("--bin").arg(bin);
    }
    if quiet {
        let output = command.output().context("spawning cargo build")?;
        if !output.status.success() {
            eprintln!("{}", String::from_utf8_lossy(&output.stderr));
            std::process::exit(output.status.code().unwrap_or(1));
        }
        return Ok(());
    }
    run_inherited(&mut command)
}

fn scenario_dir(root: &Path, scenario: &str) -> Result<PathBuf> {
    let dir = root.join(KIT_DIR).join("scenarios").join(scenario);
    if !dir.is_dir() {
        bail!("scenario not found: {}", dir.display());
    }
    Ok(dir)
}

/// The trailing agent subcommand for `agent-observer run`.
fn agent_args(root: &Path, agent: AgentKind, release: bool) -> Vec<String> {
    let profile = if release { "release" } else { "debug" };
    match agent {
        AgentKind::Baseline => vec!["rust".into(), "baseline".into()],
        AgentKind::Reference => vec!["rust".into(), "reference".into()],
        AgentKind::SacAgent => vec![
            "external".into(),
            root.join("target").join(profile).join("sac-agent").to_string_lossy().into_owned(),
        ],
        AgentKind::Python => {
            vec!["python".into(), format!("{KIT_DIR}/agent/minimal_agent.py")]
        }
    }
}

fn run_scenario(
    root: &Path,
    scenario: &str,
    agent: AgentKind,
    release: bool,
    wallclock: Option<f64>,
    quiet: bool,
) -> Result<()> {
    let bins: &[&str] = match agent {
        AgentKind::SacAgent => &["agent-observer", "sac-agent"],
        _ => &["agent-observer"],
    };
    cargo_build(root, release, bins, quiet)?;
    let profile = if release { "release" } else { "debug" };
    let mut command = Command::new(root.join("target").join(profile).join("agent-observer"));
    command
        .arg("run")
        .arg("--scenario")
        .arg(scenario_dir(root, scenario)?)
        .arg("--out")
        .arg(root.join("out").join(format!("{scenario}-{}", agent.label())))
        .current_dir(root);
    if let Some(wallclock) = wallclock {
        command.arg("--wallclock").arg(format!("{wallclock}"));
    }
    if quiet {
        command.arg("--quiet");
    }
    command.args(agent_args(root, agent, release));
    if quiet {
        let output = command.output().context("spawning agent-observer")?;
        if !output.status.success() {
            eprintln!("{}", String::from_utf8_lossy(&output.stderr));
            std::process::exit(output.status.code().unwrap_or(1));
        }
        return Ok(());
    }
    run_inherited(&mut command)
}

fn task_gate(root: &Path) -> Result<()> {
    run_inherited(Command::new("cargo").arg("test").current_dir(root))?;
    run_inherited(
        Command::new("cargo")
            .args(["test", "--release", "--", "--ignored"])
            .current_dir(root),
    )?;
    Ok(())
}

fn task_compare(root: &Path, scenario: &str, release: bool) -> Result<()> {
    cargo_build(root, release, &["agent-observer", "sac-agent"], true)?;
    println!("agent      total        required_missing  termination_reason");
    for agent in [AgentKind::Baseline, AgentKind::Reference, AgentKind::SacAgent] {
        run_scenario(root, scenario, agent, release, None, true)?;
        let report_path = root
            .join("out")
            .join(format!("{scenario}-{}", agent.label()))
            .join("score_report.json");
        let report: Value = serde_json::from_str(
            &fs::read_to_string(&report_path)
                .with_context(|| format!("reading {}", report_path.display()))?,
        )
        .with_context(|| format!("parsing {}", report_path.display()))?;
        let total = report["score"]["total"].as_f64().unwrap_or(f64::NAN);
        let missing = report["completion"]["required_missing"]
            .as_array()
            .map_or(0, Vec::len);
        let termination = report["termination_reason"].as_str().unwrap_or("?");
        println!(
            "{:<10} {:<12.6} {:<17} {termination}",
            agent.label(),
            total,
            missing
        );
    }
    Ok(())
}

/// Files that go into the submission ZIP: the manifest, the crate manifest +
/// lockfile, and all of `src/`, each paired with its relative ZIP path.
fn pack_file_set(root: &Path) -> Result<Vec<(PathBuf, String)>> {
    let mut files = Vec::new();
    for name in ["observer.project.json", "Cargo.toml", "Cargo.lock"] {
        let path = root.join(name);
        if !path.is_file() {
            bail!("pack input missing: {}", path.display());
        }
        files.push((path, name.to_string()));
    }
    let src = root.join("src");
    collect_sources(&src, &src, &mut files)?;
    files.sort_by(|a, b| a.1.cmp(&b.1));
    for (_, name) in &files {
        let relative = Path::new(name);
        let valid = !relative.is_absolute()
            && relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)));
        if !valid {
            bail!("refusing non-relative ZIP path {name:?}");
        }
        if relative.components().any(|component| {
            matches!(component, Component::Normal(part) if part == ".env")
        }) {
            bail!("refusing to pack {name:?}: .env files never ship (pack_agent.py rule)");
        }
    }
    Ok(files)
}

fn collect_sources(dir: &Path, base: &Path, files: &mut Vec<(PathBuf, String)>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            collect_sources(&path, base, files)?;
        } else if path.is_file() {
            let name = path
                .strip_prefix(base.parent().unwrap())
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            files.push((path, name));
        }
    }
    Ok(())
}

fn task_pack(root: &Path, out: &Path) -> Result<()> {
    let out = if out.is_absolute() { out.to_path_buf() } else { root.join(out) };
    // Manifest validation (pack_agent.py rules): parses, and the platform
    // contract fields are present and non-empty.
    let manifest_text = fs::read_to_string(root.join("observer.project.json"))
        .context("reading observer.project.json")?;
    let manifest: Value =
        serde_json::from_str(&manifest_text).context("observer.project.json is not valid JSON")?;
    for key in ["schema_version", "image", "run"] {
        let empty = match manifest.get(key) {
            None => true,
            Some(Value::String(text)) => text.is_empty(),
            Some(Value::Array(items)) => items.is_empty(),
            _ => false,
        };
        if empty {
            bail!("observer.project.json: {key:?} is missing or empty");
        }
    }
    let build: Vec<String> = manifest["build"]
        .as_array()
        .and_then(|steps| steps.first())
        .and_then(Value::as_array)
        .map(|step| step.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    if build.is_empty() {
        bail!("observer.project.json: \"build\" must hold at least one command array");
    }

    let files = pack_file_set(root)?;
    {
        let file = File::create(&out).with_context(|| format!("creating {}", out.display()))?;
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (path, name) in &files {
            zip.start_file(name, options)?;
            zip.write_all(&fs::read(path)?)?;
        }
        zip.finish()?;
    }
    let mut digest = Sha256::new();
    let mut bytes = File::open(&out)?;
    let mut chunk = [0u8; 65536];
    let mut size = 0u64;
    loop {
        let read = bytes.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        digest.update(&chunk[..read]);
        size += read as u64;
    }
    println!(
        "packed {} files, {size} bytes, sha256 {:x} -> {}",
        files.len(),
        digest.finalize(),
        out.display()
    );

    // Final check: the platform contract must work from the packed files
    // alone — extract to a fresh dir and run the manifest's exact build.
    let staging = std::env::temp_dir().join(format!("agent-observer-pack-{}", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;
    zip::ZipArchive::new(File::open(&out)?)
        .with_context(|| format!("reading back {}", out.display()))?
        .extract(&staging)
        .with_context(|| format!("extracting to {}", staging.display()))?;
    let mut command = Command::new(&build[0]);
    command
        .args(&build[1..])
        .arg("--target-dir")
        .arg(staging.join("target"))
        .current_dir(&staging)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    run_inherited(&mut command)?;
    let binary = staging.join("target/release/sac-agent");
    if !binary.is_file() {
        bail!("platform build check: {} was not produced", binary.display());
    }
    println!("platform build check ok: {}", binary.display());
    Ok(())
}

fn main() -> Result<()> {
    let root = repo_root();
    match Cli::parse().task {
        Task::Run { scenario, agent, release, wallclock } => {
            run_scenario(&root, &scenario, agent, release, wallclock, false)
        }
        Task::Gate => task_gate(&root),
        Task::Compare { scenario, release } => task_compare(&root, &scenario, release),
        Task::Pack { out } => task_pack(&root, &out),
        Task::Book { build } => task_book(&root, build),
    }
}

/// Build and serve the mdBook strategy notes (live reload, opens a browser);
/// `--build` does a one-shot build into book/book/ instead. `mdbook` must be
/// on PATH (`cargo install mdbook`, or a release binary in ~/.cargo/bin).
///
/// The browser is opened by us, not `mdbook serve --open`: mdbook's opener
/// only knows wslview/xdg-open, which are absent on minimal WSL setups —
/// there the reliable path is Windows interop (`cmd.exe /c start`).
fn task_book(root: &Path, build_only: bool) -> Result<()> {
    let mdbook = Command::new("mdbook")
        .arg("--version")
        .output()
        .map_err(|_| anyhow::anyhow!(
            "mdbook not found on PATH; install it with `cargo install mdbook`"
        ))?;
    if !mdbook.status.success() {
        bail!("mdbook not found on PATH; install it with `cargo install mdbook`");
    }
    let book_dir = root.join("book");
    if build_only {
        let mut command = Command::new("mdbook");
        command
            .arg("build")
            .arg(&book_dir)
            .current_dir(root)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        run_inherited(&mut command)?;
        println!("book built: {}", book_dir.join("book").join("index.html").display());
        return Ok(());
    }
    const URL: &str = "http://localhost:3000";
    let mut serve = Command::new("mdbook")
        .arg("serve")
        .arg(&book_dir)
        .current_dir(root)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning mdbook serve")?;
    // Give the server a moment to come up before pointing a browser at it.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    match browser_opener() {
        Some(mut opener) => {
            let opened = opener
                .arg(URL)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if opened {
                println!("opened {URL} in your browser");
            } else {
                println!("book is served at {URL} (browser could not be opened)");
            }
        }
        None => println!("book is served at {URL} (no browser opener found on this machine)"),
    }
    let status = serve.wait().context("waiting on mdbook serve")?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/version")
        .map(|text| text.to_lowercase().contains("microsoft"))
        .unwrap_or(false)
}

/// Best browser opener for this machine: WSL interop (the Windows default
/// browser), then the platform-native openers. On WSL, `SAC_BROWSER` can
/// force a specific browser (`edge` / `chrome` / `firefox`) instead of the
/// Windows default.
fn browser_opener() -> Option<Command> {
    if is_wsl() && on_path("cmd.exe") {
        let mut command = Command::new("cmd.exe");
        command.args(["/c", "start", ""]);
        if let Ok(browser) = std::env::var("SAC_BROWSER") {
            let exe = match browser.trim().to_lowercase().as_str() {
                "edge" => "msedge",
                "chrome" => "chrome",
                "firefox" => "firefox",
                _ => "",
            };
            if !exe.is_empty() {
                command.arg(exe);
            }
        }
        return Some(command);
    }
    if cfg!(target_os = "macos") {
        return Some(Command::new("open"));
    }
    if cfg!(target_os = "windows") {
        let mut command = Command::new("cmd");
        command.args(["/c", "start", ""]);
        return Some(command);
    }
    if on_path("xdg-open") {
        return Some(Command::new("xdg-open"));
    }
    None
}
