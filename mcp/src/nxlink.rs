// SPDX-License-Identifier: GPL-3.0-only
//! The PC side of nxlink: send an NRO and its arguments to hbmenu's NetLoader, which saves it under
//! `sdmc:/switch/` and runs it, then take the NRO's stdout, which libnx's nxlink stdio sends back to this
//! PC. A reimplementation of the protocol in switch-tools `src/nxlink.c`, as nx-hbmenu
//! `common/netloader.c` receives it (both ISC).

use std::io::Write as _;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::ZlibEncoder;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// NetLoader listens here while hbmenu shows "NetLoader active".
pub const SERVER_PORT: u16 = 28280;
/// The NRO connects back here with its stdout.
pub const CLIENT_PORT: u16 = 28771;
/// hbmenu's receive buffer: no compressed piece may be larger.
const ZLIB_CHUNK: usize = 16 * 1024;

/// Connects to hbmenu's NetLoader; refused unless hbmenu is showing "NetLoader active".
pub async fn connect(host: &str) -> std::io::Result<TcpStream> {
    let sock = TcpStream::connect((host, SERVER_PORT)).await?;
    sock.set_nodelay(true)?;
    Ok(sock)
}

/// Sends `nro` over a NetLoader connection, to be saved as `sdmc:/switch/<dest>` and started with
/// `args`. hbmenu doesn't create directories, so `dest`'s must exist.
pub async fn send(mut sock: TcpStream, nro: &[u8], dest: &str, args: &[String]) -> Result<()> {
    sock.write_all(&len32(dest.len())?).await?;
    sock.write_all(dest.as_bytes()).await?;
    sock.write_all(&len32(nro.len())?).await?;
    match sock.read_i32_le().await.context("NetLoader closed the connection")? {
        0 => {}
        -1 => bail!("hbmenu couldn't create sdmc:/switch/{dest} (does its directory exist?)"),
        -2 => bail!("not enough space on the SD card for sdmc:/switch/{dest}"),
        -3 => bail!("hbmenu is out of memory, or doesn't recognise {dest} as an NRO"),
        rc => bail!("NetLoader refused the file ({rc})"),
    }

    // One zlib stream, sent as length-prefixed pieces.
    let mut zlib = ZlibEncoder::new(Vec::new(), Compression::default());
    for chunk in nro.chunks(ZLIB_CHUNK) {
        zlib.write_all(chunk)?;
        send_pieces(&mut sock, zlib.get_mut()).await?;
    }
    send_pieces(&mut sock, &mut zlib.finish()?).await?;
    if sock.read_i32_le().await.context("NetLoader closed the connection after the file")? != 0 {
        bail!("hbmenu failed writing sdmc:/switch/{dest}");
    }

    // The arguments, each NUL-terminated; hbmenu puts the NRO's path first and our address last.
    let mut argv = Vec::new();
    for arg in args {
        argv.extend_from_slice(arg.as_bytes());
        argv.push(0);
    }
    sock.write_all(&len32(argv.len())?).await?;
    sock.write_all(&argv).await?;
    sock.shutdown().await?;
    Ok(())
}

async fn send_pieces(sock: &mut TcpStream, out: &mut Vec<u8>) -> Result<()> {
    for piece in out.chunks(ZLIB_CHUNK) {
        sock.write_all(&len32(piece.len())?).await?;
        sock.write_all(piece).await?;
    }
    out.clear();
    Ok(())
}

fn len32(n: usize) -> Result<[u8; 4]> {
    Ok(u32::try_from(n).context("too large for nxlink")?.to_le_bytes())
}

/// Waits for an NRO's stdout. Bind it before sending, so the NRO finds it.
pub struct OutputListener(TcpListener);

impl OutputListener {
    pub async fn bind() -> Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", CLIENT_PORT))
            .await
            .with_context(|| format!("listening on port {CLIENT_PORT} for the NRO's output (is another nxlink -s running?)"))?;
        Ok(OutputListener(listener))
    }

    /// The NRO's stdout connection, or `None` if it didn't connect in `wait` (it doesn't use nxlink stdio).
    pub async fn accept(self, wait: Duration) -> Result<Option<TcpStream>> {
        match tokio::time::timeout(wait, self.0.accept()).await {
            Ok(accepted) => Ok(Some(accepted?.0)),
            Err(_) => Ok(None),
        }
    }
}
