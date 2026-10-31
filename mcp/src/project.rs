// SPDX-License-Identifier: GPL-3.0-only
//! Named homebrew projects, so one call can build, deploy, run and watch a build: `~/.config/kickstand/
//! projects.json` maps a name to where the project is and how to build it, e.g.
//!
//! ```json
//! { "mygame": {
//!     "dir": "/home/me/mygame",
//!     "build": "cmake --build build-switch --target mygame_nro",
//!     "nro": "build-switch/mygame.nro",
//!     "elf": "build-switch/mygame.elf",
//!     "dest": "/switch/mygame/mygame.nro" } }
//! ```
//!
//! `nro` and `elf` are relative to `dir`; `build` runs there with `sh -c` (none: use the files as they are);
//! `dest` is where NetLoader saves the NRO, which is where the homebrew finds its data (agents may use its
//! folder without asking, and the NRO never runs in applet mode); `args` and `module`
//! (the name its backtraces use, default the ELF's file name) are optional.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::client::Client;
use crate::launch::{self, Mode, OutputLog, Program};
use crate::watchdog::{self, Settings};

#[derive(Deserialize, Clone)]
pub struct Project {
    pub dir: PathBuf,
    #[serde(default)]
    pub build: Option<String>,
    pub nro: PathBuf,
    pub elf: PathBuf,
    pub dest: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub module: Option<String>,
}

/// Build output lines shown when a build fails.
const BUILD_TAIL: usize = 60;

pub fn load_all() -> Result<BTreeMap<String, Project>> {
    let path = crate::config::config_dir()?.join("projects.json");
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {} (see `kickstand projects --help`)", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn get(name: &str) -> Result<Project> {
    let mut all = load_all()?;
    let names = all.keys().cloned().collect::<Vec<_>>().join(", ");
    all.remove(name).with_context(|| format!("no project {name:?} in projects.json (there are: {names})"))
}

impl Project {
    fn module(&self) -> String {
        self.module.clone().unwrap_or_else(|| self.elf.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default())
    }

    /// Runs the build command; on failure, the error carries the end of its output.
    async fn build(&self) -> Result<String> {
        let Some(command) = &self.build else { return Ok("no build command: using the files as they are".into()) };
        let start = Instant::now();
        let out = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&self.dir)
            .output()
            .await
            .with_context(|| format!("running {command:?}"))?;
        let log = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        let elapsed = start.elapsed().as_secs_f64();
        if !out.status.success() {
            let lines: Vec<&str> = log.lines().collect();
            let tail = lines[lines.len().saturating_sub(BUILD_TAIL)..].join("\n");
            bail!("BUILD FAILED ({}) after {elapsed:.0} s: {command}\n\n{tail}", out.status);
        }
        let warnings = log.lines().filter(|l| l.contains("warning:")).count();
        Ok(format!("built in {elapsed:.0} s ({warnings} warnings)"))
    }

    /// The NRO, after checking it was made from the ELF, and a snapshot of the ELF (`last-run.elf` next to
    /// it, as the project's own run script keeps) so a rebuild during the run can't skew symbols.
    fn prepare(&self) -> Result<(Vec<u8>, PathBuf, String)> {
        let (nro_path, elf_path) = (self.dir.join(&self.nro), self.dir.join(&self.elf));
        let nro = std::fs::read(&nro_path).with_context(|| format!("reading {}", nro_path.display()))?;
        let elf = std::fs::read(&elf_path).with_context(|| format!("reading {}", elf_path.display()))?;
        let note = match crate::memory::build_id(&elf)? {
            Some(id) => {
                let header = nro.get(0x40..0x60).context("the NRO is too short for a header")?;
                if header.get(..id.len()) != Some(&id[..]) {
                    bail!("{} wasn't made from {} (their build ids differ): did the NRO step run?", nro_path.display(), elf_path.display());
                }
                "the NRO matches the ELF (build id)"
            }
            None => "the ELF has no build id, so the NRO can't be checked against it",
        };
        let snapshot = elf_path.with_file_name("last-run.elf");
        std::fs::write(&snapshot, &elf).with_context(|| format!("writing {}", snapshot.display()))?;
        Ok((nro, snapshot, note.into()))
    }
}

pub struct TestRun {
    pub text: String,
    pub screenshot: Option<Vec<u8>>,
    /// Collects the NRO's output into the log, until it exits.
    pub collector: Option<tokio::task::JoinHandle<()>>,
    /// The ELF snapshot and module name, for later memory and symbol requests.
    pub elf: (String, String),
}

/// Builds `project` (unless `skip_build`), sends and runs it with full memory, then watches it with
/// [`watchdog::supervise`], collecting its symbolized output into `output`.
pub async fn test(
    client: &Client,
    project: &Project,
    skip_build: bool,
    args: Option<Vec<String>>,
    output: &Arc<Mutex<OutputLog>>,
    settings: &Settings,
) -> Result<TestRun> {
    let mut notes = Vec::new();
    if !skip_build {
        notes.push(project.build().await?);
    }
    let (nro, snapshot, checked) = project.prepare()?;
    notes.push(checked);
    let module = project.module();
    let symbols = crate::symbolize::Symbolizer::new(&snapshot, Some(&module))?.shared();
    let program = Program::Local { nro, dest: project.dest.clone() };
    let started = launch::run_nro(client, program, args.unwrap_or_else(|| project.args.clone()), Mode::Full).await?;
    notes.extend(started.notes);
    let collector = started.output.map(|stream| OutputLog::collect(output, stream, Some(symbols), started.pid));
    let elf = crate::memory::Elf::load(Some(&snapshot), Some(&module))?;
    let watched = collector.is_some().then_some(output);
    let report = watchdog::supervise(client, watched, &elf, settings).await?;
    Ok(TestRun {
        text: format!("{}\n\n{}", notes.join("\n"), report.text),
        screenshot: report.screenshot,
        collector,
        elf: (snapshot.to_string_lossy().into_owned(), module),
    })
}
