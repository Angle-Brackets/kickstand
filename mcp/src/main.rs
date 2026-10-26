//! switch-mcp: an MCP server (stdio) for the daemon on the Switch, plus a few CLI commands for using
//! the daemon by hand.

mod client;
mod config;
mod mock;
mod tools;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rmcp::ServiceExt;

use client::{Client, check_path};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// The Switch's IP address. Defaults to $SWITCH_IP, then ~/.config/switch-mcp/host.
    #[arg(long, global = true)]
    host: Option<String>,
    /// The daemon's port.
    #[arg(long, global = true, default_value_t = proto::DEFAULT_PORT)]
    port: u16,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server on stdio (the default).
    Serve,
    /// Create the shared token if there isn't one, and print it and where the console needs it.
    Token,
    /// Check the daemon answers.
    Ping,
    /// List a directory on the SD card.
    Ls { path: String },
    /// Print a file from the SD card.
    Cat { path: String },
    /// Copy a file from the SD card to this PC.
    Get { path: String, local: PathBuf },
    /// Upload a file to the SD card.
    Put { local: PathBuf, path: String },
    /// Serve a local directory as a fake SD card, for testing without the console.
    #[command(hide = true)]
    Mock { root: PathBuf, #[arg(long, default_value = "127.0.0.1")] bind: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Serve);

    match command {
        Command::Token => {
            let (token, path) = config::ensure_token()?;
            println!("token: {token}");
            println!("saved in {}", path.display());
            println!("on the SD card it goes in {} (just the token, one line)", proto::path::TOKEN_PATH);
            return Ok(());
        }
        Command::Mock { root, bind } => {
            let token = config::load_token()?;
            let listener = tokio::net::TcpListener::bind((bind.as_str(), cli.port)).await?;
            eprintln!("mock daemon serving {} on {}", root.display(), listener.local_addr()?);
            return mock::serve(listener, root, token).await;
        }
        _ => {}
    }

    let host = config::host(cli.host)?;
    let client = Arc::new(Client::new(format!("{host}:{}", cli.port), config::load_token()?));

    match command {
        Command::Serve => {
            let service = tools::SwitchTools::new(client).serve(rmcp::transport::stdio()).await?;
            service.waiting().await?;
        }
        Command::Ping => {
            let (version, kind) = client.hello().await?;
            let (rtt, uptime) = client.ping().await?;
            println!("daemon {version} ({kind:?}): rtt {:.1} ms, uptime {}s", rtt.as_secs_f64() * 1e3, uptime.as_secs());
        }
        Command::Ls { path } => {
            let (entries, truncated) = client.list_dir(&check_path(&path, false)?).await?;
            for e in entries {
                if e.is_dir { println!("{}/", e.name) } else { println!("{}\t{}", e.name, e.size) }
            }
            if truncated {
                println!("... (truncated)");
            }
        }
        Command::Cat { path } => {
            let path = check_path(&path, false)?;
            let size = client.stat(&path).await?.size;
            let (data, _) = client.read(&path, 0, size).await?;
            use std::io::Write;
            std::io::stdout().write_all(&data)?;
        }
        Command::Get { path, local } => {
            let path = check_path(&path, false)?;
            let size = client.stat(&path).await?.size;
            let (data, _) = client.read(&path, 0, size).await?;
            std::fs::write(&local, &data).with_context(|| format!("writing {}", local.display()))?;
            println!("{} bytes -> {}", data.len(), local.display());
        }
        Command::Put { local, path } => {
            let path = check_path(&path, true)?;
            let data = std::fs::read(&local).with_context(|| format!("reading {}", local.display()))?;
            let stats = client.upload(&path, &data).await?;
            let secs = stats.elapsed.as_secs_f64();
            println!("{} bytes in {secs:.1}s ({:.2} MB/s)", stats.size, stats.size as f64 / 1e6 / secs.max(1e-6));
        }
        Command::Token | Command::Mock { .. } => unreachable!(),
    }
    Ok(())
}

#[cfg(test)]
mod tests;
