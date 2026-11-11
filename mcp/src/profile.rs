// SPDX-License-Identifier: GPL-3.0-only
//! Sampling CPU profiler, stack folder, AI hotspot/call-tree analyzer, and interactive SVG flamegraph
//! generator.
//!
//! Supports both console-driven sampling (`Request::Profile`, protocol 29+) and a transparent PC-driven
//! fallback over `Request::Threads` (`protocol >= 20`) when talking to an older daemon.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use proto::{App, ThreadInfo};

use crate::client::Client;
use crate::memory::{Elf, Layout, Symbols};
use crate::symbolize::Symbolizer;

/// Minimum profiling duration in milliseconds.
pub const MIN_DURATION_MS: u32 = 100;
/// Maximum profiling duration in milliseconds.
pub const MAX_DURATION_MS: u32 = 15_000;
/// Default profiling duration in milliseconds (3 seconds).
pub const DEFAULT_DURATION_MS: u32 = 3_000;

/// Minimum sampling interval in milliseconds (100 Hz).
pub const MIN_INTERVAL_MS: u32 = 10;
/// Maximum sampling interval in milliseconds (5 Hz).
pub const MAX_INTERVAL_MS: u32 = 200;
/// Default sampling interval in milliseconds (20 ms = 50 Hz).
pub const DEFAULT_INTERVAL_MS: u32 = 20;

/// Default inclusive call-tree pruning threshold (percentage of total samples).
pub const DEFAULT_MIN_TREE_PCT: f64 = 2.0;
/// Default number of top exclusive hotspots shown in the AI summary.
pub const DEFAULT_MAX_HOTSPOTS: usize = 15;

/// One sampled thread stack before symbolization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    pub thread_id: u64,
    pub pc: u64,
    pub lr: u64,
    /// Frame-pointer return addresses, innermost caller first.
    pub frames: Vec<u64>,
}

impl From<ThreadInfo> for Sample {
    fn from(t: ThreadInfo) -> Self {
        Sample { thread_id: t.id, pc: t.pc, lr: t.lr, frames: t.frames }
    }
}

impl From<&ThreadInfo> for Sample {
    fn from(t: &ThreadInfo) -> Self {
        Sample { thread_id: t.id, pc: t.pc, lr: t.lr, frames: t.frames.clone() }
    }
}

/// Raw profile samples collected from either the console-side `Request::Profile` loop or the PC-side
/// `Client::threads` fallback loop.
#[derive(Debug, Clone)]
pub struct RawProfile {
    pub app: App,
    pub elapsed_ms: u32,
    pub ticks: u32,
    pub samples: Vec<Sample>,
}

/// Configuration for a profiling run.
#[derive(Debug, Clone)]
pub struct ProfileOptions {
    pub duration_ms: u32,
    pub interval_ms: u32,
    pub thread_id: Option<u64>,
    /// Minimum inclusive percentage to display a node in the AI call tree (default 2.0%).
    pub min_tree_pct: f64,
    /// Maximum exclusive hotspots to list in the AI summary (default 15).
    pub max_hotspots: usize,
    /// Optional path to write the interactive `.svg` flamegraph to. Defaults to
    /// `~/.config/kickstand/profiles/profile-<timestamp>.svg`.
    pub out: Option<PathBuf>,
}

impl Default for ProfileOptions {
    fn default() -> Self {
        ProfileOptions {
            duration_ms: DEFAULT_DURATION_MS,
            interval_ms: DEFAULT_INTERVAL_MS,
            thread_id: None,
            min_tree_pct: DEFAULT_MIN_TREE_PCT,
            max_hotspots: DEFAULT_MAX_HOTSPOTS,
            out: None,
        }
    }
}

/// A single symbolized frame in a call stack.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResolvedFrame {
    /// Function or symbol name (e.g. `collide_mesh` or `nro+0x1234`).
    pub function: String,
    /// Shortened source file and line (e.g. `src/physics.c:142`), when DWARF line info is available.
    pub location: Option<String>,
}

impl ResolvedFrame {
    pub fn new(function: impl Into<String>, location: Option<String>) -> Self {
        ResolvedFrame { function: function.into(), location }
    }

    /// Formats as `func_name [file:line]` when a source location is known, or `func_name` otherwise.
    #[allow(dead_code)]
    pub fn display(&self) -> String {
        match &self.location {
            Some(loc) if !loc.is_empty() => format!("{} [{loc}]", self.function),
            _ => self.function.clone(),
        }
    }
}

/// Ranked function hotspot with exclusive (self-time) and inclusive (total-time) sample counts.
#[derive(Debug, Clone, PartialEq)]
pub struct Hotspot {
    pub function: String,
    pub location: Option<String>,
    pub exclusive: usize,
    pub inclusive: usize,
    pub exclusive_pct: f64,
    pub inclusive_pct: f64,
}

impl Hotspot {
    pub fn display_name(&self) -> String {
        match &self.location {
            Some(loc) if !loc.is_empty() => format!("{} [{loc}]", self.function),
            _ => self.function.clone(),
        }
    }
}

/// A node in the hierarchical inclusive call tree (root -> caller -> callee).
#[derive(Debug, Clone)]
pub struct CallTreeNode {
    pub function: String,
    pub location: Option<String>,
    pub inclusive: usize,
    pub exclusive: usize,
    location_counts: HashMap<String, usize>,
    pub children: BTreeMap<String, CallTreeNode>,
}

impl CallTreeNode {
    fn new(function: String) -> Self {
        CallTreeNode {
            function,
            location: None,
            inclusive: 0,
            exclusive: 0,
            location_counts: HashMap::new(),
            children: BTreeMap::new(),
        }
    }

    fn record_location(&mut self, loc: Option<&str>) {
        let Some(loc) = loc.filter(|l| !l.is_empty()) else { return };
        let count = self.location_counts.entry(loc.to_string()).or_insert(0);
        *count += 1;
        let best = self
            .location_counts
            .iter()
            .max_by(|(loc_a, count_a), (loc_b, count_b)| count_a.cmp(count_b).then_with(|| loc_b.cmp(loc_a)))
            .map(|(k, _)| k.clone());
        self.location = best;
    }

    /// Children sorted by inclusive sample count descending, then function name ascending.
    pub fn sorted_children(&self) -> Vec<&CallTreeNode> {
        let mut children: Vec<&CallTreeNode> = self.children.values().collect();
        children.sort_by(|a, b| b.inclusive.cmp(&a.inclusive).then_with(|| a.function.cmp(&b.function)));
        children
    }

    pub fn display_name(&self) -> String {
        match &self.location {
            Some(loc) if !loc.is_empty() => format!("{} [{loc}]", self.function),
            _ => self.function.clone(),
        }
    }
}

/// Aggregated profile statistics, folded stacks, exclusive hotspots, and inclusive call tree.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ProfileAnalysis {
    pub total_samples: usize,
    pub elapsed_ms: u32,
    pub ticks: u32,
    pub effective_hz: f64,
    pub thread_count: usize,
    /// Folded stack entries `(stack_key, sample_count)` in Brendan Gregg's format.
    pub folded: Vec<(String, usize)>,
    /// Multi-line folded stack string (`root;caller;callee 42\n`).
    pub folded_text: String,
    /// Functions ranked by exclusive (self-time) samples descending.
    pub hotspots: Vec<Hotspot>,
    /// Root of the inclusive call tree (`function == "all"`).
    pub tree: CallTreeNode,
}

/// Complete output of a profiling run for both the AI agent and the developer.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ProfileOutput {
    /// Two-tier text report returned to the AI (summary stats, exclusive hotspots, inclusive call tree, and SVG path).
    pub text: String,
    /// Where the interactive `.svg` flamegraph was saved on disk.
    pub svg_path: PathBuf,
    /// Raw SVG content.
    pub svg: String,
    /// Folded stack lines (`root;caller;callee count`).
    pub folded: String,
    /// Structured analysis results.
    pub analysis: ProfileAnalysis,
}

/// Points memory/debug requests at hbloader in the Album (applet mode) if open, otherwise the application.
async fn select_target(client: &Client) -> Result<&'static str> {
    let applet = client.applet_status().await?.is_some_and(|a| a.hbl == Some(true));
    client.memory_target(applet).await?;
    Ok(if applet { "applet mode (the Album)" } else { "the application" })
}

/// Runs a CPU profile against the running homebrew/application, using native daemon-side sampling
/// (`Request::Profile`) when supported by the console, or automatically falling back to PC-driven
/// `client.threads()` sampling on older daemons.
pub async fn run(client: &Client, elf: &Elf, opts: &ProfileOptions) -> Result<ProfileOutput> {
    let duration_ms = if opts.duration_ms == 0 {
        DEFAULT_DURATION_MS
    } else {
        opts.duration_ms.clamp(MIN_DURATION_MS, MAX_DURATION_MS)
    };
    let interval_ms = if opts.interval_ms == 0 {
        DEFAULT_INTERVAL_MS
    } else {
        opts.interval_ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS)
    };

    let which = select_target(client).await?;
    let layout = Layout::fetch(client).await?;

    let raw = match client.profile(duration_ms, interval_ms, opts.thread_id).await? {
        Some(raw) => raw,
        None => sample_via_threads(client, duration_ms, interval_ms, opts.thread_id).await?,
    };

    finish(raw, which, &layout, elf, opts)
}

/// PC-driven fallback sampler using repeated [`Client::threads`] calls when the console's daemon
/// predates `Request::Profile`.
pub async fn sample_via_threads(
    client: &Client,
    duration_ms: u32,
    interval_ms: u32,
    thread_id: Option<u64>,
) -> Result<RawProfile> {
    let duration = Duration::from_millis(u64::from(duration_ms.clamp(MIN_DURATION_MS, MAX_DURATION_MS)));
    let interval = Duration::from_millis(u64::from(interval_ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS)));
    let start = Instant::now();

    let mut app_info: Option<App> = None;
    let mut samples = Vec::new();
    let mut ticks = 0u32;

    loop {
        let tick_start = Instant::now();
        match client.threads().await {
            Ok((app, threads)) => {
                app_info = Some(app);
                if let Some(tid) = thread_id {
                    if let Some(t) = threads.into_iter().find(|t| t.id == tid) {
                        samples.push(Sample::from(t));
                    } else if ticks == 0 {
                        bail!("no thread {tid:#x} in the running process");
                    }
                } else {
                    samples.extend(threads.into_iter().map(Sample::from));
                }
                ticks += 1;
            }
            Err(e) => {
                if samples.is_empty() {
                    return Err(e);
                }
                break;
            }
        }

        let elapsed = start.elapsed();
        if elapsed >= duration || samples.len() >= 500 {
            break;
        }
        let spent = tick_start.elapsed();
        if let Some(wait) = interval.checked_sub(spent) {
            let remaining = duration.saturating_sub(elapsed);
            tokio::time::sleep(wait.min(remaining)).await;
        }
    }

    let app = app_info.context("no samples collected")?;
    let elapsed_ms = start.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
    Ok(RawProfile { app, elapsed_ms, ticks, samples })
}

/// Symbolizes `raw` against `layout` and `elf`, computes hotspots and the inclusive call tree,
/// writes the interactive `.svg` flamegraph to disk, and formats the AI report.
pub fn finish(
    raw: RawProfile,
    which: &str,
    layout: &Layout,
    elf: &Elf,
    opts: &ProfileOptions,
) -> Result<ProfileOutput> {
    let symbolizer = elf.symbolizer()?;
    let symbols = elf.symbols.as_ref();

    let mut stacks = Vec::with_capacity(raw.samples.len());
    let mut threads = BTreeSet::new();
    for sample in &raw.samples {
        if sample.pc == 0 {
            continue;
        }
        threads.insert(sample.thread_id);
        let stack = unwind_sample(sample, |addr, back| {
            resolve_address(addr, back, layout, symbols, symbolizer.as_ref())
        });
        if !stack.is_empty() {
            stacks.push(stack);
        }
    }

    let analysis = analyze_stacks(&stacks, raw.elapsed_ms, raw.ticks, threads.len());
    let app_desc = crate::client::describe_app(&raw.app);
    let title = format!(
        "CPU Flamegraph — {} ({} samples, {:.2}s, {:.1} Hz)",
        app_desc,
        analysis.total_samples,
        f64::from(analysis.elapsed_ms) / 1000.0,
        analysis.effective_hz
    );
    let svg = render_svg(&analysis, &title);
    let svg_path = save_svg(&svg, opts.out.as_deref())?;
    let mismatch = layout.mismatch(symbols);
    let text = format_ai_summary(
        &analysis,
        &app_desc,
        which,
        layout.nro.is_some(),
        mismatch.as_deref(),
        &svg_path,
        opts.min_tree_pct,
        opts.max_hotspots,
    );

    Ok(ProfileOutput {
        text,
        svg_path,
        svg,
        folded: analysis.folded_text.clone(),
        analysis,
    })
}

/// Unwinds a single [`Sample`] into a root-to-leaf (outermost caller -> innermost `pc`) sequence of
/// [`ResolvedFrame`]s.
///
/// Handles AArch64 leaf functions where the immediate caller is still in `lr` (`x30`) and hasn't been
/// pushed to `[fp + 8]` yet, while avoiding duplicate self-edges when `lr` holds a return address from an
/// earlier call inside the same non-leaf function.
pub fn unwind_sample<F>(sample: &Sample, mut resolve: F) -> Vec<ResolvedFrame>
where
    F: FnMut(u64, u64) -> Vec<ResolvedFrame>,
{
    let mut stack = Vec::new();

    // 1. Frame-pointer chain is innermost-first on the wire; reverse so root/outermost caller comes first.
    for &ret in sample.frames.iter().rev() {
        if ret != 0 {
            stack.extend(resolve(ret, 4));
        }
    }

    let pc_frames = if sample.pc != 0 { resolve(sample.pc, 0) } else { Vec::new() };

    // 2. Check whether `lr` holds an unsaved caller for a leaf function at `pc`.
    if sample.lr != 0 && sample.frames.first() != Some(&sample.lr) {
        let lr_frames = resolve(sample.lr, 4);
        let lr_outer = lr_frames.first().map(|f| f.function.as_str());
        let pc_outer = pc_frames.first().map(|f| f.function.as_str());
        let prev_inner = stack.last().map(|f| f.function.as_str());

        // Include `lr` only when it resolves to a known function distinct from both `pc`'s function
        // (otherwise `lr` is just a leftover return address from a child call inside `pc`'s own function)
        // and the innermost frame-pointer caller.
        let is_meaningful = lr_outer.is_some_and(|l| !l.starts_with("0x") && l != "??");
        if is_meaningful && lr_outer != pc_outer && lr_outer != prev_inner {
            stack.extend(lr_frames);
        }
    }

    // 3. Innermost executing instruction (`pc`).
    stack.extend(pc_frames);
    stack
}

/// Resolves a single raw address into one or more [`ResolvedFrame`]s (outermost first when inlined frames
/// are expanded by DWARF). `back` is `0` for `pc` and `4` for return addresses (`lr` and `frames`).
pub fn resolve_address(
    address: u64,
    back: u64,
    layout: &Layout,
    symbols: Option<&Symbols>,
    symbolizer: Option<&Symbolizer>,
) -> Vec<ResolvedFrame> {
    if let Some((base, size)) = layout.nro
        && let Some(raw_offset) = address.checked_sub(base)
        && raw_offset < size
    {
        let offset = raw_offset.saturating_sub(back);
        if let Some(sym) = symbolizer {
            let looked = sym.lookup(offset);
            if looked != "??" {
                let fallback_sym = symbols.and_then(|s| s.name_at(offset)).map(|(n, _)| n);
                let parsed = parse_symbolizer_frames(&looked, fallback_sym, offset);
                if !parsed.is_empty() {
                    return parsed;
                }
            }
        }
        if let Some((name, _within)) = symbols.and_then(|s| s.name_at(offset)) {
            return vec![ResolvedFrame::new(name.to_string(), None)];
        }
        return vec![ResolvedFrame::new(format!("nro+{offset:#x}"), None)];
    }

    let lookup_addr = address.saturating_sub(back);
    vec![ResolvedFrame::new(format!("{lookup_addr:#x}"), None)]
}

/// Parses the string returned by [`Symbolizer::lookup`] (`"inner at file:1 / inlined into outer at file:2"`)
/// into caller-to-callee order (`outer`, then `inner`).
pub fn parse_symbolizer_frames(
    looked: &str,
    fallback_symbol: Option<&str>,
    offset: u64,
) -> Vec<ResolvedFrame> {
    let mut frames = Vec::new();
    for part in looked.split(" / inlined into ") {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (func_raw, loc_raw) = match part.rsplit_once(" at ") {
            Some((f, l)) => (f.trim(), Some(l.trim())),
            None => (part, None),
        };
        let function = if func_raw == "??" || func_raw.is_empty() {
            fallback_symbol
                .map(str::to_string)
                .unwrap_or_else(|| format!("nro+{offset:#x}"))
        } else {
            func_raw.to_string()
        };
        let location = loc_raw
            .filter(|l| !l.is_empty() && *l != "??" && !l.starts_with("??:"))
            .map(shorten_location);
        frames.push(ResolvedFrame { function, location });
    }
    // `Symbolizer::lookup` emits innermost inlined frame first; reverse to caller -> callee order.
    frames.reverse();
    frames
}

/// Shortens long build paths (`/home/user/project/src/physics.c:142` -> `src/physics.c:142`) so AI call
/// trees stay compact and readable.
pub fn shorten_location(loc: &str) -> String {
    let normalized = loc.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() >= 2 {
        format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        normalized
    }
}

/// Sanitizes a function name for Brendan Gregg's folded stack format (where `;` separates frames).
fn sanitize_folded_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            ';' => ':',
            '\n' | '\r' => ' ',
            other => other,
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() { "??".to_string() } else { trimmed.to_string() }
}

/// Aggregates symbolized root-to-leaf call stacks into folded stack counts, exclusive/inclusive hotspots,
/// and a hierarchical inclusive call tree.
pub fn analyze_stacks(
    stacks: &[Vec<ResolvedFrame>],
    elapsed_ms: u32,
    ticks: u32,
    thread_count: usize,
) -> ProfileAnalysis {
    let total_samples = stacks.len();
    let effective_hz = if elapsed_ms > 0 {
        f64::from(ticks.max(1)) * 1000.0 / f64::from(elapsed_ms)
    } else {
        0.0
    };

    let mut folded_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut exclusive_counts: HashMap<String, usize> = HashMap::new();
    let mut inclusive_counts: HashMap<String, usize> = HashMap::new();
    let mut func_locations: HashMap<String, HashMap<String, usize>> = HashMap::new();

    let mut root = CallTreeNode::new("all".to_string());
    root.inclusive = total_samples;

    for stack in stacks {
        if stack.is_empty() {
            continue;
        }

        // 1. Folded stack key (`root;caller;callee`).
        let folded_key = stack
            .iter()
            .map(|f| sanitize_folded_name(&f.function))
            .collect::<Vec<_>>()
            .join(";");
        *folded_counts.entry(folded_key).or_insert(0) += 1;

        // 2. Exclusive (self-time) hotspot on the leaf (`pc`) frame.
        let leaf = stack.last().unwrap();
        *exclusive_counts.entry(leaf.function.clone()).or_insert(0) += 1;
        if let Some(loc) = &leaf.location {
            *func_locations
                .entry(leaf.function.clone())
                .or_default()
                .entry(loc.clone())
                .or_insert(0) += 1;
        }

        // 3. Inclusive count per unique function in this sample (so recursion doesn't inflate > 100%).
        let mut seen_in_sample = BTreeSet::new();
        for frame in stack {
            if seen_in_sample.insert(frame.function.clone()) {
                *inclusive_counts.entry(frame.function.clone()).or_insert(0) += 1;
            }
            if let Some(loc) = &frame.location {
                func_locations
                    .entry(frame.function.clone())
                    .or_default()
                    .entry(loc.clone())
                    .or_insert(0);
            }
        }

        // 4. Insert path into the hierarchical inclusive call tree.
        let mut current = &mut root;
        for (i, frame) in stack.iter().enumerate() {
            let is_leaf = i + 1 == stack.len();
            if is_leaf {
                // Record location on leaf with weight.
            }
            current = current
                .children
                .entry(frame.function.clone())
                .or_insert_with(|| CallTreeNode::new(frame.function.clone()));
            current.inclusive += 1;
            if is_leaf {
                current.exclusive += 1;
            }
            current.record_location(frame.location.as_deref());
        }
    }

    // Build folded stack list sorted by count descending, then stack string ascending.
    let mut folded: Vec<(String, usize)> = folded_counts.into_iter().collect();
    folded.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut folded_text = String::new();
    for (stack_str, count) in &folded {
        let _ = writeln!(folded_text, "{stack_str} {count}");
    }

    // Build ranked hotspot list (every function with exclusive > 0, plus top inclusive if needed).
    let mut all_funcs: Vec<String> = inclusive_counts.keys().cloned().collect();
    all_funcs.sort();

    let denom = (total_samples.max(1)) as f64;
    let mut hotspots: Vec<Hotspot> = all_funcs
        .into_iter()
        .map(|function| {
            let exclusive = exclusive_counts.get(&function).copied().unwrap_or(0);
            let inclusive = inclusive_counts.get(&function).copied().unwrap_or(0);
            let location = func_locations.get(&function).and_then(|locs| {
                locs.iter()
                    .max_by(|(la, ca), (lb, cb)| ca.cmp(cb).then_with(|| lb.cmp(la)))
                    .map(|(l, _)| l.clone())
            });
            Hotspot {
                function,
                location,
                exclusive,
                inclusive,
                exclusive_pct: (exclusive as f64) * 100.0 / denom,
                inclusive_pct: (inclusive as f64) * 100.0 / denom,
            }
        })
        .collect();

    hotspots.sort_by(|a, b| {
        b.exclusive
            .cmp(&a.exclusive)
            .then_with(|| b.inclusive.cmp(&a.inclusive))
            .then_with(|| a.function.cmp(&b.function))
    });

    ProfileAnalysis {
        total_samples,
        elapsed_ms,
        ticks,
        effective_hz,
        thread_count: thread_count.max(usize::from(total_samples > 0)),
        folded,
        folded_text,
        hotspots,
        tree: root,
    }
}

/// Formats the token-efficient AI profile report with summary stats, Top Exclusive (Self-Time) Hotspots,
/// and the pruned Inclusive Call Tree.
#[allow(clippy::too_many_arguments)]
pub fn format_ai_summary(
    analysis: &ProfileAnalysis,
    app_desc: &str,
    which: &str,
    has_nro: bool,
    mismatch_warning: Option<&str>,
    svg_path: &Path,
    min_tree_pct: f64,
    max_hotspots: usize,
) -> String {
    let mut out = String::new();
    if let Some(warning) = mismatch_warning {
        out.push_str(warning);
    }
    let _ = writeln!(out, "Profiled in {which}: {app_desc}");
    let _ = writeln!(
        out,
        "Samples: {} ({} ticks over {:.2} s, {:.1} Hz, {} {})",
        analysis.total_samples,
        analysis.ticks,
        f64::from(analysis.elapsed_ms) / 1000.0,
        analysis.effective_hz,
        analysis.thread_count,
        if analysis.thread_count == 1 { "thread" } else { "threads" }
    );
    let _ = writeln!(out, "Flamegraph SVG: {}", svg_path.display());

    if !has_nro {
        out.push_str("(no NRO found in memory, so addresses are raw hex)\n");
    }

    if analysis.total_samples == 0 {
        out.push_str("\nNo stack samples were collected.\n");
        return out;
    }

    // 1. Top Exclusive (Self-Time) Hotspots (where `pc` was directly sampled).
    out.push_str("\nTop Exclusive (Self-Time) Hotspots:\n");
    let leaf_hotspots: Vec<&Hotspot> = analysis
        .hotspots
        .iter()
        .filter(|h| h.exclusive > 0)
        .take(max_hotspots.max(1))
        .collect();
    for h in leaf_hotspots {
        let _ = writeln!(
            out,
            "  {:5.1}% ({:>3} samples, {:5.1}% incl)  {}",
            h.exclusive_pct,
            h.exclusive,
            h.inclusive_pct,
            h.display_name()
        );
    }

    // 2. Hierarchical Inclusive Call Tree above threshold.
    let threshold_pct = min_tree_pct.clamp(0.0, 100.0);
    let _ = writeln!(out, "\nInclusive Call Tree (>= {threshold_pct:.1}% of samples):");
    let children = analysis.tree.sorted_children();
    let visible: Vec<&CallTreeNode> = children
        .into_iter()
        .filter(|c| (c.inclusive as f64) * 100.0 / (analysis.total_samples as f64) >= threshold_pct)
        .collect();

    if visible.is_empty() {
        for c in analysis.tree.sorted_children().into_iter().take(5) {
            render_tree_node(&mut out, c, analysis.total_samples, threshold_pct, "", true, 0);
        }
    } else {
        for (i, child) in visible.iter().enumerate() {
            let is_last = i + 1 == visible.len();
            render_tree_node(&mut out, child, analysis.total_samples, threshold_pct, "", is_last, 0);
        }
    }

    out
}

fn render_tree_node(
    out: &mut String,
    node: &CallTreeNode,
    total_samples: usize,
    min_pct: f64,
    prefix: &str,
    is_last: bool,
    depth: usize,
) {
    let pct = (node.inclusive as f64) * 100.0 / (total_samples.max(1) as f64);
    let connector = if depth == 0 {
        ""
    } else if is_last {
        "└─ "
    } else {
        "├─ "
    };

    if node.exclusive > 0 && node.exclusive < node.inclusive {
        let self_pct = (node.exclusive as f64) * 100.0 / (total_samples.max(1) as f64);
        let _ = writeln!(
            out,
            "  {prefix}{connector}{pct:5.1}% ({}) {} (self: {self_pct:.1}%)",
            node.inclusive,
            node.display_name()
        );
    } else {
        let _ = writeln!(
            out,
            "  {prefix}{connector}{pct:5.1}% ({}) {}",
            node.inclusive,
            node.display_name()
        );
    }

    if depth >= 32 {
        return;
    }

    let child_prefix = if depth == 0 {
        String::new()
    } else if is_last {
        format!("{prefix}   ")
    } else {
        format!("{prefix}│  ")
    };

    let visible_children: Vec<&CallTreeNode> = node
        .sorted_children()
        .into_iter()
        .filter(|c| (c.inclusive as f64) * 100.0 / (total_samples.max(1) as f64) >= min_pct)
        .collect();

    for (i, child) in visible_children.iter().enumerate() {
        let last = i + 1 == visible_children.len();
        render_tree_node(out, child, total_samples, min_pct, &child_prefix, last, depth + 1);
    }
}

/// Saves `svg` to `out_path` if given, or to `~/.config/kickstand/profiles/profile-<timestamp>.svg`.
pub fn save_svg(svg: &str, out_path: Option<&Path>) -> Result<PathBuf> {
    let path = match out_path {
        Some(p) => p.to_path_buf(),
        None => {
            let dir = crate::config::config_dir()?.join("profiles");
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            dir.join(format!("profile-{stamp}.svg"))
        }
    };
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating profile directory {}", parent.display()))?;
    }
    std::fs::write(&path, svg).with_context(|| format!("writing flamegraph SVG to {}", path.display()))?;
    Ok(path)
}

/// Renders a self-contained, interactive SVG flamegraph with warm flame colors, hover tooltips,
/// click-to-zoom, and search highlighting. Zero external crate dependencies.
pub fn render_svg(analysis: &ProfileAnalysis, title: &str) -> String {
    const WIDTH: f64 = 1200.0;
    const XPAD: f64 = 10.0;
    const CHART_W: f64 = WIDTH - 2.0 * XPAD;
    const ROW_H: f64 = 17.0;
    const HEADER_H: f64 = 66.0;
    const FOOTER_H: f64 = 34.0;

    let max_depth = tree_depth(&analysis.tree).max(1);
    let chart_h = (max_depth as f64) * ROW_H;
    let total_h = HEADER_H + chart_h + FOOTER_H;
    let total_samples = analysis.total_samples.max(1);

    let mut rects = Vec::new();
    layout_flame_rects(
        &analysis.tree,
        0,
        max_depth,
        0.0,
        1.0,
        total_samples,
        HEADER_H,
        ROW_H,
        XPAD,
        CHART_W,
        &mut rects,
    );

    let mut svg = String::with_capacity(rects.len() * 280 + 4096);
    let _ = write!(
        svg,
        r##"<?xml version="1.0" encoding="utf-8" standalone="no"?>
<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">
<svg version="1.1" width="{WIDTH}" height="{total_h}" viewBox="0 0 {WIDTH} {total_h}" xmlns="http://www.w3.org/2000/svg">
<style type="text/css">
  svg {{ font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 12px; user-select: none; }}
  .bg {{ fill: #fdfaf3; }}
  .title {{ font-size: 16px; font-weight: bold; fill: #222; }}
  .subtitle {{ font-size: 12px; fill: #555; }}
  .btn {{ cursor: pointer; fill: #eee4d3; stroke: #cbbba0; stroke-width: 1; rx: 4; }}
  .btn:hover {{ fill: #e2d3ba; }}
  .btn-text {{ font-size: 12px; fill: #222; pointer-events: none; }}
  .fg-frame {{ cursor: pointer; }}
  .fg-frame rect {{ stroke: #ffffff; stroke-width: 0.6; rx: 2; transition: opacity 0.1s; }}
  .fg-frame:hover rect {{ stroke: #111111; stroke-width: 1.2; }}
  .fg-frame text {{ fill: #111111; font-size: 11.5px; pointer-events: none; }}
  .status-bar {{ fill: #f3ece0; stroke: #dfd3be; stroke-width: 1; }}
  .details {{ font-size: 12px; fill: #111; }}
  .matched {{ font-size: 12px; fill: #b30086; font-weight: bold; }}
</style>
<rect class="bg" x="0" y="0" width="{WIDTH}" height="{total_h}"/>
<text class="title" x="{XPAD}" y="26">{title_escaped}</text>
<text class="subtitle" x="{XPAD}" y="46">Click any frame to zoom in • Click "Reset Zoom" or "all" to zoom out • Click "Search" (or press /) to highlight functions</text>
<g id="reset-btn" onclick="resetZoom()" style="display:none">
  <rect class="btn" x="945" y="12" width="110" height="26"/>
  <text class="btn-text" x="1000" y="29" text-anchor="middle">Reset Zoom</text>
</g>
<g id="search-btn" onclick="searchPrompt()">
  <rect class="btn" x="1065" y="12" width="125" height="26"/>
  <text id="search-label" class="btn-text" x="1127" y="29" text-anchor="middle">Search</text>
</g>
<text id="match-pct" class="matched" x="935" y="29" text-anchor="end"></text>
<g id="frames">
"##,
        title_escaped = xml_escape(title)
    );

    for r in &rects {
        let full_label = match &r.location {
            Some(loc) => format!("{} [{}]", r.function, loc),
            None => r.function.clone(),
        };
        let tooltip = format!(
            "{} ({} {}, {:.2}%)",
            full_label,
            r.samples,
            if r.samples == 1 { "sample" } else { "samples" },
            r.pct
        );
        let color = if r.depth == 0 {
            "rgb(235,175,95)".to_string()
        } else {
            flame_color(&r.function)
        };
        let shown_text = fit_label(&r.function, r.w);
        let _ = writeln!(
            svg,
            r#"<g class="fg-frame" data-name="{name}" data-full="{full}" data-samples="{samples}" data-pct="{pct:.2}" data-x="{nx:.6}" data-w="{nw:.6}" data-depth="{depth}" data-color="{color}">
  <title>{title}</title>
  <rect x="{x:.2}" y="{y:.2}" width="{w:.2}" height="{h:.2}" fill="{color}"/>
  <text x="{tx:.2}" y="{ty:.2}">{text}</text>
</g>"#,
            name = xml_escape(&r.function),
            full = xml_escape(&full_label),
            samples = r.samples,
            pct = r.pct,
            nx = r.norm_x,
            nw = r.norm_w,
            depth = r.depth,
            color = color,
            title = xml_escape(&tooltip),
            x = r.x,
            y = r.y,
            w = r.w,
            h = ROW_H - 1.0,
            tx = r.x + 3.0,
            ty = r.y + 12.0,
            text = xml_escape(&shown_text),
        );
    }

    let footer_y = HEADER_H + chart_h + 6.0;
    let footer_text_y = footer_y + 18.0;
    let _ = write!(
        svg,
        r##"</g>
<rect class="status-bar" x="{XPAD}" y="{footer_y:.1}" width="{CHART_W}" height="24" rx="3"/>
<text id="details" class="details" x="{details_x}" y="{footer_text_y:.1}">Hover over a frame for details ({total_samples} total samples)</text>
<script type="text/ecmascript"><![CDATA[
(function() {{
  var XPAD = {XPAD};
  var CHART_W = {CHART_W};
  var TOTAL = {total_samples};
  var details = document.getElementById("details");
  var resetBtn = document.getElementById("reset-btn");
  var searchLabel = document.getElementById("search-label");
  var matchPct = document.getElementById("match-pct");
  var frames = Array.prototype.slice.call(document.querySelectorAll(".fg-frame"));
  var currentTerm = "";

  function fitText(name, width) {{
    var maxChars = Math.floor((width - 6) / 6.8);
    if (maxChars < 3) return "";
    if (name.length <= maxChars) return name;
    return name.slice(0, maxChars - 1) + "…";
  }}

  frames.forEach(function(g) {{
    g.addEventListener("mouseenter", function() {{
      var full = g.getAttribute("data-full");
      var s = g.getAttribute("data-samples");
      var p = g.getAttribute("data-pct");
      details.textContent = "Function: " + full + " — " + s + (s === "1" ? " sample (" : " samples (") + p + "%)";
    }});
    g.addEventListener("mouseleave", function() {{
      details.textContent = " ";
    }});
    g.addEventListener("click", function(e) {{
      e.stopPropagation();
      zoomTo(g);
    }});
  }});

  window.zoomTo = function(target) {{
    var tx = parseFloat(target.getAttribute("data-x"));
    var tw = parseFloat(target.getAttribute("data-w"));
    var td = parseInt(target.getAttribute("data-depth"), 10);
    if (tw <= 0) return;
    resetBtn.style.display = (td === 0 && tw >= 0.9999) ? "none" : "inline";

    frames.forEach(function(g) {{
      var x = parseFloat(g.getAttribute("data-x"));
      var w = parseFloat(g.getAttribute("data-w"));
      var d = parseInt(g.getAttribute("data-depth"), 10);
      var rect = g.querySelector("rect");
      var text = g.querySelector("text");
      var name = g.getAttribute("data-name");

      var eps = 1e-7;
      var isAncestor = d < td && x <= tx + eps && (x + w) >= (tx + tw) - eps;
      var isSelfOrDescendant = d >= td && x + eps >= tx && (x + w) <= (tx + tw) + eps;

      if (!isAncestor && !isSelfOrDescendant) {{
        g.style.display = "none";
        return;
      }}
      g.style.display = "inline";
      var newNx = isAncestor ? 0.0 : (x - tx) / tw;
      var newNw = isAncestor ? 1.0 : w / tw;
      var px = XPAD + newNx * CHART_W;
      var pw = Math.max(0.5, newNw * CHART_W);
      rect.setAttribute("x", px.toFixed(2));
      rect.setAttribute("width", pw.toFixed(2));
      rect.style.opacity = isAncestor ? "0.65" : "1.0";
      text.setAttribute("x", (px + 3).toFixed(2));
      text.textContent = fitText(name, pw);
    }});
  }};

  window.resetZoom = function() {{
    if (frames.length > 0) zoomTo(frames[0]);
  }};

  window.searchPrompt = function() {{
    if (currentTerm) {{
      clearSearch();
      return;
    }}
    var term = prompt("Search function name (substring or regex):", "");
    if (!term) return;
    applySearch(term);
  }};

  function clearSearch() {{
    currentTerm = "";
    searchLabel.textContent = "Search";
    matchPct.textContent = "";
    frames.forEach(function(g) {{
      var rect = g.querySelector("rect");
      rect.setAttribute("fill", g.getAttribute("data-color"));
    }});
  }}

  function applySearch(term) {{
    currentTerm = term;
    var re;
    try {{ re = new RegExp(term, "i"); }} catch (_) {{ re = new RegExp(term.replace(/[.*+?^${{}}()|[\]\\]/g, "\\$&"), "i"); }}
    var matchedIntervals = [];
    frames.forEach(function(g) {{
      var d = parseInt(g.getAttribute("data-depth"), 10);
      var name = g.getAttribute("data-full");
      var rect = g.querySelector("rect");
      if (d > 0 && re.test(name)) {{
        rect.setAttribute("fill", "rgb(230,0,230)");
        var x = parseFloat(g.getAttribute("data-x"));
        var w = parseFloat(g.getAttribute("data-w"));
        matchedIntervals.push([x, x + w]);
      }} else {{
        rect.setAttribute("fill", g.getAttribute("data-color"));
      }}
    }});
    matchedIntervals.sort(function(a, b) {{ return a[0] - b[0]; }});
    var covered = 0.0;
    var curEnd = -1.0;
    for (var i = 0; i < matchedIntervals.length; i++) {{
      var s = matchedIntervals[i][0], e = matchedIntervals[i][1];
      if (s >= curEnd) {{
        covered += (e - s);
        curEnd = e;
      }} else if (e > curEnd) {{
        covered += (e - curEnd);
        curEnd = e;
      }}
    }}
    searchLabel.textContent = "Clear";
    matchPct.textContent = "Matched: " + (covered * 100).toFixed(1) + "%";
  }}

  window.addEventListener("keydown", function(e) {{
    if (e.key === "Escape") {{
      resetZoom();
    }} else if (e.key === "/") {{
      e.preventDefault();
      searchPrompt();
    }}
  }});
}})();
]]></script>
</svg>
"##,
        details_x = XPAD + 8.0,
    );

    svg
}

struct FlameRect {
    function: String,
    location: Option<String>,
    samples: usize,
    pct: f64,
    depth: usize,
    norm_x: f64,
    norm_w: f64,
    x: f64,
    y: f64,
    w: f64,
}

fn tree_depth(node: &CallTreeNode) -> usize {
    1 + node.children.values().map(tree_depth).max().unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
fn layout_flame_rects(
    node: &CallTreeNode,
    depth: usize,
    max_depth: usize,
    norm_x: f64,
    norm_w: f64,
    total_samples: usize,
    header_h: f64,
    row_h: f64,
    xpad: f64,
    chart_w: f64,
    out: &mut Vec<FlameRect>,
) {
    let w = norm_w * chart_w;
    if w < 0.1 {
        return;
    }
    let x = xpad + norm_x * chart_w;
    // Root ("all", depth 0) sits at the bottom of the flame chart; children stack upward.
    let y = header_h + ((max_depth - 1 - depth) as f64) * row_h;
    let pct = (node.inclusive as f64) * 100.0 / (total_samples.max(1) as f64);

    out.push(FlameRect {
        function: node.function.clone(),
        location: node.location.clone(),
        samples: node.inclusive,
        pct,
        depth,
        norm_x,
        norm_w,
        x,
        y,
        w,
    });

    let mut child_norm_x = norm_x;
    for child in node.sorted_children() {
        let child_norm_w = (child.inclusive as f64) / (total_samples.max(1) as f64);
        layout_flame_rects(
            child,
            depth + 1,
            max_depth,
            child_norm_x,
            child_norm_w,
            total_samples,
            header_h,
            row_h,
            xpad,
            chart_w,
            out,
        );
        child_norm_x += child_norm_w;
    }
}

/// Deterministic warm flame color (`rgb(205..250, 50..190, 20..65)`) hashed from the function name.
pub fn flame_color(name: &str) -> String {
    let mut h1: u64 = 0xcbf2_9ce4_8422_2325;
    let mut h2: u64 = 0x8422_2325_cbf2_9ce4;
    for b in name.bytes() {
        h1 ^= u64::from(b);
        h1 = h1.wrapping_mul(0x0100_0000_01b3);
        h2 = h2.wrapping_add(u64::from(b).wrapping_mul(131));
        h2 ^= h2 >> 17;
    }
    let v1 = (h1 & 0xFFFF) as f64 / 65535.0;
    let v2 = ((h1 >> 16) & 0xFFFF) as f64 / 65535.0;
    let v3 = (h2 & 0xFFFF) as f64 / 65535.0;

    let r = 205 + (v1 * 45.0).round() as u8;
    let g = 50 + (v2 * 140.0).round() as u8;
    let b = 20 + (v3 * 45.0).round() as u8;
    format!("rgb({r},{g},{b})")
}

fn fit_label(name: &str, width_px: f64) -> String {
    let max_chars = ((width_px - 6.0) / 6.8).floor() as usize;
    if max_chars < 3 {
        return String::new();
    }
    let char_count = name.chars().count();
    if char_count <= max_chars {
        return name.to_string();
    }
    let mut truncated: String = name.chars().take(max_chars - 1).collect();
    truncated.push('…');
    truncated
}

pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(func: &str, loc: Option<&str>) -> ResolvedFrame {
        ResolvedFrame::new(func, loc.map(str::to_string))
    }

    #[test]
    fn stack_folding_collapses_identical_paths_and_sanitizes_semicolons() {
        let stacks = vec![
            vec![
                frame("main", Some("src/main.rs:10")),
                frame("physics::step", Some("src/physics.rs:40")),
                frame("collide_mesh", Some("src/physics.rs:142")),
            ],
            vec![
                frame("main", Some("src/main.rs:10")),
                frame("physics::step", Some("src/physics.rs:42")),
                frame("collide_mesh", Some("src/physics.rs:145")),
            ],
            vec![
                frame("main", Some("src/main.rs:12")),
                frame("render;pass", Some("src/render.rs:88")),
            ],
        ];

        let analysis = analyze_stacks(&stacks, 1000, 50, 2);
        assert_eq!(analysis.total_samples, 3);
        assert_eq!(analysis.thread_count, 2);
        assert!((analysis.effective_hz - 50.0).abs() < 1e-6);

        assert_eq!(
            analysis.folded,
            vec![
                ("main;physics::step;collide_mesh".to_string(), 2),
                ("main;render:pass".to_string(), 1),
            ]
        );
        assert_eq!(
            analysis.folded_text,
            "main;physics::step;collide_mesh 2\nmain;render:pass 1\n"
        );
    }

    #[test]
    fn exclusive_and_inclusive_hotspots_are_calculated_accurately() {
        // 4 samples total:
        // - 2 in main -> physics -> collide_mesh
        // - 1 in main -> physics -> broadphase
        // - 1 in main -> physics (self sample inside physics)
        let stacks = vec![
            vec![
                frame("main", Some("src/main.c:10")),
                frame("physics", Some("src/physics.c:50")),
                frame("collide_mesh", Some("src/physics.c:142")),
            ],
            vec![
                frame("main", Some("src/main.c:10")),
                frame("physics", Some("src/physics.c:50")),
                frame("collide_mesh", Some("src/physics.c:142")),
            ],
            vec![
                frame("main", Some("src/main.c:10")),
                frame("physics", Some("src/physics.c:55")),
                frame("broadphase", Some("src/physics.c:90")),
            ],
            vec![
                frame("main", Some("src/main.c:10")),
                frame("physics", Some("src/physics.c:60")),
            ],
        ];

        let analysis = analyze_stacks(&stacks, 2000, 100, 1);
        assert_eq!(analysis.total_samples, 4);

        // Top exclusive hotspot must be `collide_mesh` (2/4 = 50.0%).
        let top = &analysis.hotspots[0];
        assert_eq!(top.function, "collide_mesh");
        assert_eq!(top.location.as_deref(), Some("src/physics.c:142"));
        assert_eq!(top.exclusive, 2);
        assert_eq!(top.inclusive, 2);
        assert!((top.exclusive_pct - 50.0).abs() < 1e-6);

        // `physics` has 1 exclusive sample (25%) and 4 inclusive samples (100%).
        let phys = analysis.hotspots.iter().find(|h| h.function == "physics").unwrap();
        assert_eq!(phys.exclusive, 1);
        assert_eq!(phys.inclusive, 4);
        assert!((phys.exclusive_pct - 25.0).abs() < 1e-6);
        assert!((phys.inclusive_pct - 100.0).abs() < 1e-6);

        // `main` has 0 exclusive samples and 4 inclusive samples (100%).
        let main_h = analysis.hotspots.iter().find(|h| h.function == "main").unwrap();
        assert_eq!(main_h.exclusive, 0);
        assert_eq!(main_h.inclusive, 4);
        assert!((main_h.inclusive_pct - 100.0).abs() < 1e-6);
    }

    #[test]
    fn recursive_stacks_count_inclusive_once_per_sample() {
        let stacks = vec![vec![
            frame("main", None),
            frame("traverse_bvh", Some("src/bvh.rs:20")),
            frame("traverse_bvh", Some("src/bvh.rs:25")),
            frame("traverse_bvh", Some("src/bvh.rs:30")),
        ]];

        let analysis = analyze_stacks(&stacks, 100, 5, 1);
        let bvh = analysis.hotspots.iter().find(|h| h.function == "traverse_bvh").unwrap();
        assert_eq!(bvh.inclusive, 1);
        assert_eq!(bvh.exclusive, 1);
        assert!((bvh.inclusive_pct - 100.0).abs() < 1e-6);
    }

    #[test]
    fn aarch64_leaf_lr_recovery_and_clobbered_lr_deduplication() {
        let map = |addr: u64, _back: u64| -> Vec<ResolvedFrame> {
            let name = match addr {
                0x1000..=0x10FF => "main",
                0x2000..=0x20FF => "caller_func",
                0x3000..=0x30FF => "leaf_func",
                _ => "??",
            };
            vec![ResolvedFrame::new(name, None)]
        };

        // Case 1: `pc` is in `leaf_func` (0x3010), `lr` is in `caller_func` (0x2040),
        // and `frames[0]` is in `main` (0x1020). `caller_func` should be recovered from `lr`!
        let leaf_sample = Sample {
            thread_id: 1,
            pc: 0x3010,
            lr: 0x2040,
            frames: vec![0x1020],
        };
        let unwound = unwind_sample(&leaf_sample, map);
        let names: Vec<&str> = unwound.iter().map(|f| f.function.as_str()).collect();
        assert_eq!(names, vec!["main", "caller_func", "leaf_func"]);

        // Case 2: `pc` is in `caller_func` (0x2050) after a child call returned, so `lr` is ALSO
        // inside `caller_func` (0x2030), while `frames[0]` is `main` (0x1020).
        // `lr` must NOT create a bogus `caller_func -> caller_func` self-call.
        let non_leaf_sample = Sample {
            thread_id: 1,
            pc: 0x2050,
            lr: 0x2030,
            frames: vec![0x1020],
        };
        let unwound2 = unwind_sample(&non_leaf_sample, map);
        let names2: Vec<&str> = unwound2.iter().map(|f| f.function.as_str()).collect();
        assert_eq!(names2, vec!["main", "caller_func"]);
    }

    #[test]
    fn ai_call_tree_formats_hierarchy_and_prunes_below_threshold() {
        let mut stacks = Vec::new();
        for _ in 0..70 {
            stacks.push(vec![
                frame("main", Some("src/main.rs:10")),
                frame("physics_step", Some("src/physics.rs:45")),
                frame("collide_mesh", Some("src/physics.rs:142")),
            ]);
        }
        for _ in 0..29 {
            stacks.push(vec![
                frame("main", Some("src/main.rs:10")),
                frame("render_frame", Some("src/render.rs:22")),
            ]);
        }
        // 1% cold branch that should be pruned at >= 2.0% threshold:
        stacks.push(vec![
            frame("main", Some("src/main.rs:10")),
            frame("rare_housekeeping", Some("src/util.rs:5")),
        ]);

        let analysis = analyze_stacks(&stacks, 2000, 100, 1);
        let report = format_ai_summary(
            &analysis,
            "MyGame (pid 99)",
            "the application",
            true,
            None,
            Path::new("/tmp/profile-test.svg"),
            2.0,
            10,
        );

        assert!(report.contains("Top Exclusive (Self-Time) Hotspots:"), "{report}");
        assert!(report.contains("70.0% ( 70 samples,  70.0% incl)  collide_mesh [src/physics.rs:142]"), "{report}");
        assert!(report.contains("29.0% ( 29 samples,  29.0% incl)  render_frame [src/render.rs:22]"), "{report}");
        assert!(report.contains("Inclusive Call Tree (>= 2.0% of samples):"), "{report}");
        assert!(report.contains("100.0% (100) main [src/main.rs:10]"), "{report}");
        assert!(report.contains("├─  70.0% (70) physics_step [src/physics.rs:45]"), "{report}");
        assert!(report.contains("└─  29.0% (29) render_frame [src/render.rs:22]"), "{report}");
        // Pruned from the inclusive tree because 1.0% < 2.0%:
        assert!(!report.contains("├─   1.0% (1) rare_housekeeping"), "{report}");
        assert!(!report.contains("└─   1.0% (1) rare_housekeeping"), "{report}");
    }

    #[test]
    fn svg_flamegraph_is_valid_interactive_and_escapes_xml_special_chars() {
        let stacks = vec![
            vec![
                frame("main", Some("src/main.rs:1")),
                frame("<alloc::vec::Vec<T> as core::ops::Drop>::drop & \"fast\"", Some("src/lib.rs:42")),
            ],
            vec![
                frame("main", Some("src/main.rs:1")),
                frame("audio::mix", Some("src/audio.rs:19")),
            ],
        ];

        let analysis = analyze_stacks(&stacks, 1000, 50, 1);
        let svg = render_svg(&analysis, "Flamegraph <Test & \"Verify\">");

        assert!(svg.starts_with("<?xml version=\"1.0\""), "missing XML header");
        assert!(svg.contains("<svg "), "missing <svg> root");
        assert!(svg.trim_end().ends_with("</svg>"), "missing </svg> closing tag");
        assert!(svg.contains("Flamegraph &lt;Test &amp; &quot;Verify&quot;&gt;"));
        assert!(svg.contains("&lt;alloc::vec::Vec&lt;T&gt; as core::ops::Drop&gt;::drop &amp; &quot;fast&quot;"));
        assert!(svg.contains("window.zoomTo = function"));
        assert!(svg.contains("window.resetZoom = function"));
        assert!(svg.contains("window.searchPrompt = function"));
    }

    #[test]
    fn symbolizer_inline_frames_reverse_to_caller_callee_order() {
        let looked = "inner_leaf at /build/game/src/math.rs:12 / inlined into outer_caller at /build/game/src/physics.rs:88";
        let frames = parse_symbolizer_frames(looked, Some("outer_caller"), 0x1234);
        assert_eq!(
            frames,
            vec![
                ResolvedFrame::new("outer_caller", Some("src/physics.rs:88".to_string())),
                ResolvedFrame::new("inner_leaf", Some("src/math.rs:12".to_string())),
            ]
        );
    }
}
