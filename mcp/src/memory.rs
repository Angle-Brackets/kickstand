// SPDX-License-Identifier: GPL-3.0-only
//! The running application's memory: its map (and where in it the homebrew's NRO is), addresses written as
//! expressions (`0x…`, `nro+0x…`, `heap+…`, a symbol from the NRO's ELF, `[pointer]+offset`), and the bytes
//! as a hexdump or typed values. The NRO's ELF is linked at 0, so a symbol's address is its offset in the
//! NRO. Region kinds are libnx's `MemoryType` (`svc.h`), the NRO header layout is switchbrew's.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use proto::MemoryMap;

use crate::client::Client;

/// Most bytes one read returns.
pub const MAX_READ: usize = 1 << 20;

/// The map plus where the NRO is: (base, image size + bss), read from its header.
pub struct Layout {
    pub map: MemoryMap,
    pub nro: Option<(u64, u64)>,
}

impl Layout {
    pub async fn fetch(client: &Client) -> Result<Self> {
        let map = client.memory_map().await?;
        let nro = match map.regions.iter().find(|r| r.nro) {
            // The header: 0x18 size (text + ro + data), 0x38 bss size.
            Some(region) => {
                let header = client.read_memory(region.base, 0x40).await?;
                let word = |at: usize| u32::from_le_bytes(header[at..at + 4].try_into().unwrap()) as u64;
                Some((region.base, word(0x18) + word(0x38)))
            }
            None => None,
        };
        Ok(Layout { map, nro })
    }

    fn nro_base(&self) -> Result<u64> {
        self.nro.map(|(base, _)| base).ok_or_else(|| {
            anyhow!("no NRO in the application's memory (is it a game, or hbmenu with nothing loaded? hbmenu itself is an NRO)")
        })
    }

    /// `address` as `nro+0x…` and the symbol it falls in, when it's inside the NRO.
    pub fn describe(&self, address: u64, symbols: Option<&Symbols>) -> Option<String> {
        let (base, size) = self.nro?;
        let offset = address.checked_sub(base).filter(|&o| o < size)?;
        let mut out = format!("nro+{offset:#x}");
        if let Some((name, within)) = symbols.and_then(|s| s.name_at(offset)) {
            let _ = if within == 0 { write!(out, " {name}") } else { write!(out, " {name}+{within:#x}") };
        }
        Some(out)
    }

    pub fn render(&self, symbols: Option<&Symbols>) -> String {
        let m = &self.map;
        let mut out = format!("{}\n", crate::client::describe_app(&m.app));
        if let Some((base, len)) = m.heap {
            let _ = writeln!(out, "heap at {base:#x} ({})", size(len));
        }
        match self.nro {
            Some((base, len)) => {
                let _ = writeln!(out, "NRO at {base:#x}, {} with bss: `nro+0x…` and symbols are relative to it", size(len));
            }
            None => out.push_str("no NRO found (not homebrew, or hbmenu between NROs)\n"),
        }
        out.push_str("\nstart              end                size     perm  kind\n");
        for r in &m.regions {
            let perm = format!(
                "{}{}{}",
                if r.perm & 1 != 0 { 'r' } else { '-' },
                if r.perm & 2 != 0 { 'w' } else { '-' },
                if r.perm & 4 != 0 { 'x' } else { '-' }
            );
            let _ = write!(out, "{:#018x} {:#018x} {:>8} {perm}   {}", r.base, r.base + r.size, size(r.size), kind(r.state));
            if let Some(at) = self.describe(r.base, symbols) {
                let _ = write!(out, "  {at}");
            }
            if r.nro {
                out.push_str("  <- NRO header");
            }
            out.push('\n');
        }
        if m.truncated {
            let _ = writeln!(out, "(only the first {} regions)", proto::MAX_MEMORY_REGIONS);
        }
        out
    }
}

fn size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 && b % (1 << 30) == 0 => format!("{} GiB", b >> 30),
        b if b >= 1 << 20 => format!("{:.1} MiB", b as f64 / (1 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KiB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

/// libnx `MemoryType`, the low byte of the state.
fn kind(state: u32) -> &'static str {
    const KINDS: [&str; 0x18] = [
        "free", "io", "static", "code", "code data", "heap", "shared", "alias", "alias code", "alias code data", "ipc",
        "stack", "thread local", "transferred", "shared transferred", "shared code", "inaccessible", "non-secure ipc",
        "non-device ipc", "kernel", "generated code", "code out", "coverage", "insecure",
    ];
    KINDS.get((state & 0xFF) as usize).copied().unwrap_or("?")
}

/// The NRO's symbol table, by name (and demangled name) and by address.
pub struct Symbols {
    by_name: HashMap<String, (u64, u64)>,
    /// (address, size, name), sorted by address.
    sorted: Vec<(u64, u64, String)>,
}

impl Symbols {
    pub fn load(elf: &Path) -> Result<Self> {
        use object::{Object, ObjectSymbol, SymbolKind};
        let data = std::fs::read(elf).with_context(|| format!("reading {}", elf.display()))?;
        let file = object::File::parse(&*data).with_context(|| format!("parsing {}", elf.display()))?;
        let mut by_name = HashMap::new();
        let mut sorted = Vec::new();
        for sym in file.symbols() {
            let (Ok(name), true) = (sym.name(), matches!(sym.kind(), SymbolKind::Data | SymbolKind::Text | SymbolKind::Tls)) else {
                continue;
            };
            if name.is_empty() || sym.is_undefined() {
                continue;
            }
            let demangled = addr2line::demangle_auto(name.into(), None).into_owned();
            by_name.insert(name.to_string(), (sym.address(), sym.size()));
            by_name.entry(demangled.clone()).or_insert((sym.address(), sym.size()));
            sorted.push((sym.address(), sym.size(), demangled));
        }
        if sorted.is_empty() {
            bail!("{} has no symbols", elf.display());
        }
        sorted.sort();
        Ok(Symbols { by_name, sorted })
    }

    pub fn get(&self, name: &str) -> Option<(u64, u64)> {
        self.by_name.get(name).copied()
    }

    /// The symbol containing `offset` (in the NRO), and how far into it.
    pub fn name_at(&self, offset: u64) -> Option<(&str, u64)> {
        let i = self.sorted.partition_point(|s| s.0 <= offset).checked_sub(1)?;
        let (address, size, name) = &self.sorted[i];
        let within = offset - address;
        (within < (*size).max(1)).then_some((name.as_str(), within))
    }
}

/// Evaluates an address expression: a number (`0x…` or decimal), `nro`, the module name (e.g. the ELF's
/// `Paperboat`, as backtraces write it), `heap`, or a symbol, then any `+`/`-` offsets. `[expr]` reads the
/// 64-bit pointer at `expr`.
pub async fn resolve(client: &Client, layout: &Layout, symbols: Option<&Symbols>, module: Option<&str>, expr: &str) -> Result<u64> {
    let expr = expr.trim();
    if expr.is_empty() {
        bail!("empty address");
    }
    // The last + or - outside brackets splits off an offset.
    let mut depth = 0i32;
    let mut split = None;
    for (i, c) in expr.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            '+' | '-' if depth == 0 && i > 0 => split = Some((i, c)),
            _ => {}
        }
    }
    if let Some((i, op)) = split
        && let Ok(offset) = number(expr[i + 1..].trim())
    {
        let base = Box::pin(resolve(client, layout, symbols, module, &expr[..i])).await?;
        return Ok(if op == '+' { base.wrapping_add(offset) } else { base.wrapping_sub(offset) });
    }
    if let Some(inner) = expr.strip_prefix('[').and_then(|e| e.strip_suffix(']')) {
        let at = Box::pin(resolve(client, layout, symbols, module, inner)).await?;
        let bytes = client.read_memory(at, 8).await.with_context(|| format!("reading the pointer at {at:#x}"))?;
        return Ok(u64::from_le_bytes(bytes[..8].try_into().unwrap()));
    }
    if let Ok(n) = number(expr) {
        return Ok(n);
    }
    let lower = expr.to_ascii_lowercase();
    if lower == "nro" || module.is_some_and(|m| m.eq_ignore_ascii_case(expr)) {
        return layout.nro_base();
    }
    if lower == "heap" {
        return layout.map.heap.map(|(base, _)| base).context("no heap mapped");
    }
    let Some(symbols) = symbols else {
        bail!("{expr:?} isn't a number, nro, heap or [pointer]; give the NRO's ELF (elf) to use symbol names");
    };
    let (offset, _) = symbols.get(expr).with_context(|| format!("no symbol {expr:?} in the ELF"))?;
    Ok(layout.nro_base()? + offset)
}

fn number(s: &str) -> Result<u64> {
    let s = s.trim().replace('_', "");
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).map_err(Into::into),
        None => s.parse().map_err(Into::into),
    }
}

/// How to show memory, and how to encode a value for a write.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Hex,
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    /// 64-bit pointers, described when they point into the NRO.
    Ptr,
    /// A NUL-terminated string.
    Str,
}

impl Format {
    pub const NAMES: &str = "hex, u8, u16, u32, u64, i8, i16, i32, i64, f32, f64, ptr, str";

    pub fn parse(name: &str) -> Result<Self> {
        Ok(match name.to_ascii_lowercase().as_str() {
            "hex" | "bytes" => Format::Hex,
            "u8" => Format::U8,
            "u16" => Format::U16,
            "u32" => Format::U32,
            "u64" => Format::U64,
            "i8" | "s8" => Format::I8,
            "i16" | "s16" => Format::I16,
            "i32" | "s32" => Format::I32,
            "i64" | "s64" => Format::I64,
            "f32" | "float" => Format::F32,
            "f64" | "double" => Format::F64,
            "ptr" | "pointer" => Format::Ptr,
            "str" | "string" => Format::Str,
            other => bail!("unknown format {other:?}: use one of {}", Format::NAMES),
        })
    }

    pub fn width(self) -> usize {
        match self {
            Format::Hex | Format::U8 | Format::I8 | Format::Str => 1,
            Format::U16 | Format::I16 => 2,
            Format::U32 | Format::I32 | Format::F32 => 4,
            Format::U64 | Format::I64 | Format::F64 | Format::Ptr => 8,
        }
    }

    /// `value` encoded for a write: a number for the numeric formats, text for str (written with a
    /// NUL), hex digits for hex (spaces allowed).
    pub fn encode(self, value: &str) -> Result<Vec<u8>> {
        let v = value.trim();
        let int = |bits: u32, signed: bool| -> Result<Vec<u8>> {
            let n: i128 = match v.strip_prefix('-') {
                Some(rest) if signed => -(number(rest)? as i128),
                _ => number(v)? as i128,
            };
            let (min, max) = if signed { (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1) } else { (0, (1i128 << bits) - 1) };
            if !(min..=max).contains(&n) {
                bail!("{v} doesn't fit in {bits} bits");
            }
            Ok((n as u64).to_le_bytes()[..bits as usize / 8].to_vec())
        };
        Ok(match self {
            Format::Hex => {
                let digits: String = v.chars().filter(|c| !c.is_whitespace()).collect();
                let digits = digits.strip_prefix("0x").unwrap_or(&digits);
                if !digits.len().is_multiple_of(2) {
                    bail!("hex bytes need an even number of digits");
                }
                (0..digits.len()).step_by(2).map(|i| u8::from_str_radix(&digits[i..i + 2], 16)).collect::<Result<_, _>>()?
            }
            Format::U8 => int(8, false)?,
            Format::U16 => int(16, false)?,
            Format::U32 => int(32, false)?,
            Format::U64 | Format::Ptr => int(64, false)?,
            Format::I8 => int(8, true)?,
            Format::I16 => int(16, true)?,
            Format::I32 => int(32, true)?,
            Format::I64 => int(64, true)?,
            Format::F32 => v.parse::<f32>()?.to_le_bytes().to_vec(),
            Format::F64 => v.parse::<f64>()?.to_le_bytes().to_vec(),
            Format::Str => v.bytes().chain([0]).collect(),
        })
    }
}

/// `data` read from `address`, shown as `format`.
pub fn render(data: &[u8], address: u64, format: Format, layout: &Layout, symbols: Option<&Symbols>) -> String {
    let mut out = String::new();
    match format {
        Format::Hex => {
            for (i, row) in data.chunks(16).enumerate() {
                let _ = write!(out, "{:#018x} ", address + 16 * i as u64);
                for (j, b) in row.iter().enumerate() {
                    let _ = write!(out, "{}{b:02x}", if j == 8 { "  " } else { " " });
                }
                let pad = 16 - row.len();
                out.push_str(&" ".repeat(pad * 3 + usize::from(row.len() <= 8)));
                let text: String = row.iter().map(|&b| if (0x20..0x7F).contains(&b) { b as char } else { '.' }).collect();
                let _ = writeln!(out, "  |{text}|");
            }
        }
        Format::Str => {
            let end = data.iter().position(|&b| b == 0);
            let _ = writeln!(out, "{:?}{}", String::from_utf8_lossy(&data[..end.unwrap_or(data.len())]), if end.is_none() { " (no NUL yet)" } else { "" });
        }
        _ => {
            let width = format.width();
            let per_line = if format == Format::Ptr { 1 } else { 16 / width.max(2) };
            for (i, line) in data.chunks(width * per_line).enumerate() {
                let _ = write!(out, "{:#018x}:", address + (i * width * per_line) as u64);
                for v in line.chunks_exact(width) {
                    let mut word = [0u8; 8];
                    word[..width].copy_from_slice(v);
                    let u = u64::from_le_bytes(word);
                    let sign = |bits: u32| ((u << (64 - bits)) as i64) >> (64 - bits);
                    let _ = match format {
                        Format::U8 | Format::U16 | Format::U32 | Format::U64 => write!(out, " {u}"),
                        Format::I8 => write!(out, " {}", sign(8)),
                        Format::I16 => write!(out, " {}", sign(16)),
                        Format::I32 => write!(out, " {}", sign(32)),
                        Format::I64 => write!(out, " {}", u as i64),
                        Format::F32 => write!(out, " {}", f32::from_bits(u as u32)),
                        Format::F64 => write!(out, " {}", f64::from_bits(u)),
                        _ => {
                            let what = layout.describe(u, symbols).map(|d| format!(" ({d})")).unwrap_or_default();
                            write!(out, " {u:#x}{what}")
                        }
                    };
                }
                out.push('\n');
            }
        }
    }
    out
}

/// The NRO's ELF, if given: its symbols, and its module name (the file name without .elf by default).
pub struct Elf {
    pub symbols: Option<Symbols>,
    pub module: Option<String>,
}

impl Elf {
    pub fn load(elf: Option<&Path>, module: Option<&str>) -> Result<Self> {
        let Some(elf) = elf else { return Ok(Elf { symbols: None, module: module.map(str::to_string) }) };
        let module = module.map(str::to_string).or_else(|| elf.file_stem().map(|s| s.to_string_lossy().into_owned()));
        Ok(Elf { symbols: Some(Symbols::load(elf)?), module })
    }
}

/// Points memory requests at the homebrew being worked on: hbloader in the Album (applet mode) if it's
/// there, otherwise the application. Returns which, for the output.
async fn target(client: &Client) -> Result<&'static str> {
    let applet = client.applet_status().await?.is_some_and(|a| a.hbl == Some(true));
    client.memory_target(applet).await?;
    Ok(if applet { "applet mode (the Album)" } else { "the application" })
}

/// The application's memory map, as text.
pub async fn map(client: &Client, elf: &Elf) -> Result<String> {
    let which = target(client).await?;
    Ok(format!("in {which}: {}", Layout::fetch(client).await?.render(elf.symbols.as_ref())))
}

/// Reads `len` bytes at the address `expr` and shows them as `format` (`len` 0: one value, or 256 bytes
/// for a string).
pub async fn peek(client: &Client, elf: &Elf, expr: &str, len: usize, format: Format) -> Result<String> {
    target(client).await?;
    let layout = Layout::fetch(client).await?;
    let address = resolve(client, &layout, elf.symbols.as_ref(), elf.module.as_deref(), expr).await?;
    let len = match len {
        0 if format == Format::Str => 256,
        0 if format == Format::Hex => 64,
        0 => format.width(),
        n => n.min(MAX_READ),
    };
    let data = client.read_memory(address, len).await?;
    let mut out = format!("{len} bytes at {address:#x}");
    if let Some(at) = layout.describe(address, elf.symbols.as_ref()) {
        let _ = write!(out, " ({at})");
    }
    out.push('\n');
    out.push_str(&render(&data, address, format, &layout, elf.symbols.as_ref()));
    Ok(out)
}

/// Writes `value` encoded as `format` at the address `expr`; returns what was there before and after.
pub async fn poke(client: &Client, elf: &Elf, expr: &str, value: &str, format: Format) -> Result<String> {
    target(client).await?;
    let layout = Layout::fetch(client).await?;
    let address = resolve(client, &layout, elf.symbols.as_ref(), elf.module.as_deref(), expr).await?;
    let data = format.encode(value)?;
    let before = client.read_memory(address, data.len()).await?;
    client.write_memory(address, &data).await?;
    let after = client.read_memory(address, data.len()).await?;
    let shown = if format == Format::Str { Format::Hex } else { format };
    let mut out = format!("wrote {} bytes at {address:#x}", data.len());
    if let Some(at) = layout.describe(address, elf.symbols.as_ref()) {
        let _ = write!(out, " ({at})");
    }
    let _ = write!(
        out,
        "\nbefore: {}after:  {}",
        render(&before, address, shown, &layout, None).split_once(' ').map_or("", |(_, v)| v).trim_start_matches(':'),
        render(&after, address, shown, &layout, None).split_once(' ').map_or("", |(_, v)| v).trim_start_matches(':'),
    );
    Ok(out)
}
