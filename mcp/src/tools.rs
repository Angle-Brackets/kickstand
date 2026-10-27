// SPDX-License-Identifier: GPL-3.0-only
//! The MCP tools. Each one is a thin wrapper over [`Client`]; failures come back as tool errors
//! (`isError: true`) with the daemon's message, so the agent can read and react to them.

use std::fmt::Write as _;
use std::sync::Arc;

use base64::Engine;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::client::{Client, check_path};

/// Default and maximum bytes `read_file` returns inline; use `download` for more.
const READ_DEFAULT: u64 = 64 * 1024;
const READ_MAX: u64 = 1024 * 1024;

#[derive(Clone)]
pub struct Tools {
    client: Arc<Client>,
    tool_router: ToolRouter<Self>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct PathArg {
    /// Absolute path from the SD card root, e.g. /switch/paperboat or /atmosphere/crash_reports.
    pub path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ReadArgs {
    /// Absolute path from the SD card root.
    pub path: String,
    /// Byte offset to start at (default 0).
    pub offset: Option<u64>,
    /// Bytes to read (default 65536, max 1048576).
    pub length: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct DownloadArgs {
    /// File on the SD card, absolute from its root.
    pub path: String,
    /// Where to save it on this PC.
    pub local_path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct UploadArgs {
    /// File on this PC to send.
    pub local_path: String,
    /// Destination on the SD card. Must be under /switch/paperboat/, /switch/ttydrecomp/ or /kickstand/.
    pub remote_path: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct DeleteArgs {
    /// File or directory on the SD card. Must be under /switch/paperboat/, /switch/ttydrecomp/ or /kickstand/.
    pub path: String,
    /// Delete a directory and everything in it (default false: only files and empty directories).
    pub recursive: Option<bool>,
}

fn ok(text: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

fn fail(e: anyhow::Error) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))])
}

#[tool_router]
impl Tools {
    pub fn new(client: Arc<Client>) -> Self {
        Tools { client, tool_router: Self::tool_router() }
    }

    #[tool(description = "Check the daemon on the Switch is reachable. Reports round-trip time, console uptime, daemon version and whether it runs as the sysmodule or the test NRO.")]
    async fn ping(&self) -> CallToolResult {
        let result = async {
            let (version, kind) = self.client.hello().await?;
            let (rtt, uptime) = self.client.ping().await?;
            anyhow::Ok(format!(
                "daemon {version} ({kind:?}) at {}: rtt {:.1} ms, console uptime {}s",
                self.client.addr(),
                rtt.as_secs_f64() * 1000.0,
                uptime.as_secs()
            ))
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "List a directory on the Switch's SD card. Keys, NAND backups and /Nintendo are not readable.")]
    async fn list_dir(&self, Parameters(args): Parameters<PathArg>) -> CallToolResult {
        let result = async {
            let path = check_path(&args.path, false)?;
            let (mut entries, truncated) = self.client.list_dir(&path).await?;
            entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
            let mut out = format!("{path} ({} entries)\n", entries.len());
            for e in &entries {
                if e.is_dir {
                    let _ = writeln!(out, "  {}/", e.name);
                } else {
                    let _ = writeln!(out, "  {}  {} bytes", e.name, e.size);
                }
            }
            if truncated {
                let _ = writeln!(out, "  ... truncated at {} entries", proto::MAX_DIR_ENTRIES);
            }
            anyhow::Ok(out)
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Size and type of a file or directory on the Switch's SD card.")]
    async fn stat(&self, Parameters(args): Parameters<PathArg>) -> CallToolResult {
        let result = async {
            let path = check_path(&args.path, false)?;
            let e = self.client.stat(&path).await?;
            anyhow::Ok(if e.is_dir { format!("{path}: directory") } else { format!("{path}: file, {} bytes", e.size) })
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Read part of a file on the Switch's SD card, such as a log or a crash report in /atmosphere/crash_reports. Text comes back as text; binary data as base64.")]
    async fn read_file(&self, Parameters(args): Parameters<ReadArgs>) -> CallToolResult {
        let result = async {
            let path = check_path(&args.path, false)?;
            let offset = args.offset.unwrap_or(0);
            let len = args.length.unwrap_or(READ_DEFAULT).min(READ_MAX);
            let (data, size) = self.client.read(&path, offset, len).await?;
            let end = offset + data.len() as u64;
            let header = format!("{path}: bytes {offset}..{end} of {size}");
            anyhow::Ok(match String::from_utf8(data) {
                Ok(text) => format!("{header}\n{text}"),
                Err(e) => {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(e.into_bytes());
                    format!("{header} (binary, base64)\n{b64}")
                }
            })
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Copy a whole file from the Switch's SD card to this PC.")]
    async fn download(&self, Parameters(args): Parameters<DownloadArgs>) -> CallToolResult {
        let result = async {
            let path = check_path(&args.path, false)?;
            let size = self.client.stat(&path).await?.size;
            let (data, _) = self.client.read(&path, 0, size).await?;
            tokio::fs::write(&args.local_path, &data).await?;
            anyhow::Ok(format!("saved {path} ({} bytes) to {}", data.len(), args.local_path))
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Upload a file from this PC to the Switch's SD card, e.g. a fresh .nro build. Verified with CRC-32 before it replaces the old file. Only /switch/paperboat/, /switch/ttydrecomp/ and /kickstand/ are writable.")]
    async fn upload(&self, Parameters(args): Parameters<UploadArgs>) -> CallToolResult {
        let result = async {
            let remote = check_path(&args.remote_path, true)?;
            let data = tokio::fs::read(&args.local_path).await?;
            let stats = self.client.upload(&remote, &data).await?;
            let secs = stats.elapsed.as_secs_f64();
            anyhow::Ok(format!(
                "uploaded {} -> {remote}: {} bytes in {secs:.1}s ({:.2} MB/s)",
                args.local_path,
                stats.size,
                stats.size as f64 / 1e6 / secs.max(1e-6)
            ))
        };
        result.await.map_or_else(fail, ok)
    }

    #[tool(description = "Delete a file or directory on the Switch's SD card, e.g. old builds or test output. Only inside /switch/paperboat/, /switch/ttydrecomp/ and /kickstand/; directories must be empty unless recursive is true.")]
    async fn delete(&self, Parameters(args): Parameters<DeleteArgs>) -> CallToolResult {
        let result = async {
            let path = check_path(&args.path, true)?;
            let was_dir = self.client.delete(&path, args.recursive.unwrap_or(false)).await?;
            anyhow::Ok(format!("deleted {path}{}", if was_dir { " (directory)" } else { "" }))
        };
        result.await.map_or_else(fail, ok)
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "kickstand",
    instructions = "Tools for a real Nintendo Switch running the kickstand daemon. Paths are absolute from the SD card root. Start with ping."
)]
impl ServerHandler for Tools {}
