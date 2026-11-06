use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn run(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("Starting {command:?}"))?;
    ensure!(status.success(), "Command failed ({status}): {command:?}");
    Ok(())
}

fn cargo(root: &Path) -> Command {
    let mut command = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command.current_dir(root);
    command
}

fn metadata(root: &Path) -> Result<Value> {
    let output = cargo(root)
        .args([
            "metadata",
            "--offline",
            "--locked",
            "--format-version",
            "1",
            "--filter-platform",
            "x86_64-pc-windows-msvc",
        ])
        .output()?;
    ensure!(
        output.status.success(),
        "Cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_owned()
}

fn license_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["LICENSE", "LICENCE", "COPYING", "COPYRIGHT", "NOTICE"]
        .iter()
        .any(|prefix| {
            upper == *prefix
                || upper
                    .strip_prefix(prefix)
                    .is_some_and(|tail| tail.starts_with(['.', '-', '_']))
        })
}

fn licenses(root: &Path, meta: &Value, destination: &Path) -> Result<usize> {
    let packages = meta["packages"]
        .as_array()
        .context("Cargo packages missing")?;
    let members: HashSet<_> = meta["workspace_members"]
        .as_array()
        .context("Cargo workspace missing")?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let nodes: HashMap<_, _> = meta["resolve"]["nodes"]
        .as_array()
        .context("Cargo graph missing")?
        .iter()
        .filter_map(|node| node["id"].as_str().map(|id| (id, node)))
        .collect();
    let mut pending: VecDeque<&str> = packages
        .iter()
        .filter(|p| {
            matches!(
                p["name"].as_str(),
                Some("stepthumbnail-app" | "stepthumbnail-shell" | "stepthumbnail-host")
            )
        })
        .filter_map(|p| p["id"].as_str())
        .collect();
    ensure!(!pending.is_empty(), "Runtime packages not found");
    let mut reached = HashSet::new();
    while let Some(id) = pending.pop_front() {
        if !reached.insert(id) {
            continue;
        }
        if let Some(deps) = nodes.get(id).and_then(|node| node["deps"].as_array()) {
            for dep in deps {
                if dep["dep_kinds"]
                    .as_array()
                    .is_some_and(|kinds| kinds.iter().any(|k| k["kind"] != "dev"))
                {
                    if let Some(id) = dep["pkg"].as_str() {
                        pending.push_back(id);
                    }
                }
            }
        }
    }
    fs::create_dir_all(destination)?;
    let mut inventory = BTreeMap::new();
    let mut missing = Vec::new();
    for p in packages {
        let id = p["id"].as_str().unwrap_or_default();
        if !reached.contains(id) || members.contains(id) {
            continue;
        }
        let name = text(p, "name");
        let version = text(p, "version");
        let label = format!("{name}-{version}");
        let folder = Path::new(
            p["manifest_path"]
                .as_str()
                .context("Manifest path missing")?,
        )
        .parent()
        .context("Manifest parent missing")?;
        let extra = root.join("licenses/extra").join(&label);
        let mut sources = BTreeMap::new();
        for entry in fs::read_dir(folder)?.flatten() {
            let path = entry.path();
            if path.is_file() && license_name(&entry.file_name().to_string_lossy()) {
                sources.insert(entry.file_name(), path);
            }
        }
        if let Some(file) = p["license_file"].as_str() {
            let path = folder.join(file);
            if path.is_file() {
                sources.insert(path.file_name().unwrap().to_owned(), path);
            }
        }
        if extra.is_dir() {
            for entry in fs::read_dir(extra)?
                .flatten()
                .filter(|e| e.path().is_file())
            {
                sources.insert(entry.file_name(), entry.path());
            }
        }
        let mut license = text(p, "license");
        let mut repository = p["repository"].clone();
        let mut authors = p["authors"].clone();
        if name == "truck-assembly" && version == "0.1.0" {
            license = "Apache-2.0".into();
            repository = json!("https://github.com/ricosjp/truck");
            authors = json!(["Truck contributors"]);
        }
        if sources.is_empty()
            && matches!(
                license.as_str(),
                "Apache-2.0" | "MIT OR Apache-2.0" | "Apache-2.0 OR MIT"
            )
        {
            sources.insert(
                "LICENSE-APACHE".into(),
                root.join("licenses/look/LICENSE-APACHE"),
            );
        }
        if sources.is_empty() {
            missing.push(format!("{label}: {license}"));
            continue;
        }
        let out = destination.join(&label);
        fs::create_dir_all(&out)?;
        let mut files = Vec::new();
        for (file, path) in sources {
            fs::copy(&path, out.join(&file))
                .with_context(|| format!("Copying {}", path.display()))?;
            files.push(format!("{label}/{}", file.to_string_lossy()));
        }
        inventory.insert(
            label,
            json!({
                "name": name, "version": version, "license": license, "authors": authors,
                "repository": repository, "source": p["source"], "texts": files,
            }),
        );
    }
    ensure!(
        missing.is_empty(),
        "Missing license texts: {}",
        missing.join("; ")
    );
    let entries: Vec<_> = inventory.into_values().collect();
    fs::write(
        destination.join("packages.json"),
        serde_json::to_vec_pretty(&entries)?,
    )?;
    fs::copy(
        root.join("licenses/THIRD-PARTY.txt"),
        destination.join("THIRD-PARTY.txt"),
    )?;
    Ok(entries.len())
}

fn compiler(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit.or_else(|| env::var_os("INNO_COMPILER").map(PathBuf::from)) {
        ensure!(
            path.is_file(),
            "Inno compiler not found: {}",
            path.display()
        );
        return Ok(path);
    }
    let mut candidates: Vec<_> = env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join("ISCC.exe"))
        .collect();
    for key in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(root) = env::var_os(key) {
            for folder in ["Inno Setup 7", "Inno Setup 6"] {
                candidates.push(Path::new(&root).join(folder).join("ISCC.exe"));
            }
        }
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .context("Install Inno Setup, put ISCC.exe on PATH, or pass --compiler <ISCC.exe>")
}

fn main() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let mut args = env::args().skip(1);
    let action = args.next().unwrap_or_else(|| "help".into());
    if action == "help" || action == "--help" {
        println!(
            "cargo xtask dist [--version x.y.z] [--compiler <ISCC.exe>] [--skip-build]\ncargo xtask licenses"
        );
        return Ok(());
    }
    ensure!(
        matches!(action.as_str(), "dist" | "licenses"),
        "Unknown task: {action}"
    );
    let mut version = None;
    let mut inno = None;
    let mut skip_build = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--version" => version = Some(args.next().context("Expected version")?),
            "--compiler" => {
                inno = Some(PathBuf::from(
                    args.next().context("Expected compiler path")?,
                ))
            }
            "--skip-build" => skip_build = true,
            _ => bail!("Unknown option: {arg}"),
        }
    }
    if action == "dist" && !skip_build {
        run(cargo(root).args([
            "build",
            "--release",
            "--locked",
            "-p",
            "stepthumbnail-app",
            "-p",
            "stepthumbnail-shell",
            "-p",
            "stepthumbnail-host",
        ]))?;
    }
    let meta = metadata(root)?;
    let target = PathBuf::from(
        meta["target_directory"]
            .as_str()
            .context("Cargo target missing")?,
    );
    let app_version = meta["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "stepthumbnail-app")
        .context("App package missing")?["version"]
        .as_str()
        .unwrap()
        .to_owned();
    let version = version.unwrap_or_else(|| app_version.clone());
    ensure!(
        version.split('.').count() == 3
            && version
                .split('.')
                .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())),
        "Version must be x.y.z"
    );
    ensure!(
        version == app_version,
        "Release version {version} differs from Cargo package version {app_version}"
    );
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let stage = target
        .join("stepthumbs-package")
        .join(format!("{version}-{}-{nonce}", std::process::id()));
    let count = licenses(root, &meta, &stage.join("licenses"))?;
    println!(
        "Collected licenses for {count} packages in {}",
        stage.display()
    );
    if action == "licenses" {
        return Ok(());
    }
    let binaries = target.join("release");
    let mut digest = Sha256::new();
    for file in [
        "StepThumbs.exe",
        "StepThumbsEngine.exe",
        "StepThumbsShell.dll",
    ] {
        digest.update(
            fs::read(binaries.join(file))
                .with_context(|| format!("Missing {file}; build release binaries first"))?,
        );
    }
    let build = format!("{:x}", digest.finalize());
    let inno = compiler(inno)?;
    run(Command::new(inno)
        .current_dir(root)
        .arg(format!("/DAppVersion={version}"))
        .arg(format!("/DBuildId={}", &build[..12]))
        .arg(format!("/DBinaryDir={}", binaries.display()))
        .arg(format!("/DLicenseDir={}", stage.join("licenses").display()))
        .arg(root.join("installer/StepThumbs.iss")))?;
    let installer = root
        .join("dist")
        .join(format!("StepThumbs-{version}-Setup.exe"));
    let hash = format!("{:x}", Sha256::digest(fs::read(&installer)?));
    fs::write(
        root.join("dist/SHA256SUMS.txt"),
        format!(
            "{hash}  {}\n",
            installer.file_name().unwrap().to_string_lossy()
        ),
    )?;
    println!("Installer: {}", installer.display());
    Ok(())
}
