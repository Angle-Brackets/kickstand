// SPDX-License-Identifier: GPL-3.0-only
//! `kickstand watch`: a web page on this PC showing the console's screen live (an MJPEG stream of
//! screenshots) and what's running, so a person can follow what an agent does, while maintaining a
//! rolling PC-side [`VideoRingBuffer`] of recent frames.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::capture::{Shot, VideoRingBuffer, motion_text_summary};
use crate::client::Client;

const PAGE: &str = include_str!("watch.html");

/// The newest frame, shared by every viewer; captured only while someone watches, plus a PC-side
/// rolling frame ring buffer holding the last ~10 s of streamed frames.
struct Frames {
    latest: watch::Sender<Option<Arc<Vec<u8>>>>,
    ring: Mutex<VideoRingBuffer>,
}

pub async fn serve(client: Arc<Client>, bind: &str, port: u16, fps: u32) -> Result<()> {
    let listener = TcpListener::bind((bind, port)).await?;
    eprintln!("watching the console at http://{}:{port}/ (Ctrl-C to stop)", if bind == "0.0.0.0" { "<this PC>" } else { bind });
    let (latest, _) = watch::channel(None);
    let frames = Arc::new(Frames {
        latest,
        ring: Mutex::new(VideoRingBuffer::new()),
    });
    tokio::spawn(capture(client.clone(), frames.clone(), fps.clamp(1, 10)));
    loop {
        let (sock, _) = listener.accept().await?;
        let (client, frames) = (client.clone(), frames.clone());
        tokio::spawn(async move {
            let _ = handle(sock, &client, &frames).await;
        });
    }
}

/// Takes screenshots while anyone is subscribed to the stream and pushes them to the PC ring buffer.
async fn capture(client: Arc<Client>, frames: Arc<Frames>, fps: u32) {
    let period = Duration::from_millis(1000 / u64::from(fps));
    let stream_start = Instant::now();
    loop {
        if frames.latest.receiver_count() == 0 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        let started = tokio::time::Instant::now();
        let at = stream_start.elapsed();
        match client.screenshot().await {
            Ok(jpeg) => {
                frames.ring.lock().unwrap().push(Shot {
                    at,
                    jpeg: jpeg.clone(),
                });
                frames.latest.send_replace(Some(Arc::new(jpeg)));
            }
            Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
        }
        tokio::time::sleep(period.saturating_sub(started.elapsed())).await;
    }
}

async fn handle(mut sock: TcpStream, client: &Client, frames: &Frames) -> Result<()> {
    let mut buf = vec![0; 2048];
    let n = sock.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let path = request.split_whitespace().nth(1).unwrap_or("/");
    match path {
        "/" => respond(&mut sock, "text/html; charset=utf-8", PAGE.as_bytes()).await,
        "/status" => {
            let (app, awake) = match client.status().await {
                Ok(status) => {
                    let mut app = match (&status.applet, &status.application) {
                        (Some(a), _) if a.hbl == Some(true) => "homebrew in applet mode (the Album)".to_string(),
                        (_, Some(app)) => crate::client::describe_app(app),
                        _ => "home menu (nothing running)".to_string(),
                    };
                    if let Some(ms) = status.unlocking_ms {
                        app.push_str(&format!(" · getting past the lock screen ({} s)", ms.div_ceil(1000)));
                    }
                    let awake = match status.keep_awake_s {
                        0 => "keep-awake off".to_string(),
                        s => format!("kept awake for {}m more", s / 60),
                    };
                    (app, awake)
                }
                Err(e) => (format!("console not answering: {e:#}"), String::new()),
            };
            let (buffered_frames, buffered_bytes) = {
                let ring = frames.ring.lock().unwrap();
                (ring.len(), ring.total_bytes())
            };
            let body = serde_json::json!({
                "app": app,
                "awake": awake,
                "ring_frames": buffered_frames,
                "ring_bytes": buffered_bytes,
            })
            .to_string();
            respond(&mut sock, "application/json", body.as_bytes()).await
        }
        "/clip-summary" => {
            let shots = frames.ring.lock().unwrap().shots();
            let summary = if shots.is_empty() {
                "no frames buffered yet".to_string()
            } else {
                motion_text_summary(&shots).unwrap_or_else(|e| format!("summary error: {e:#}"))
            };
            respond(&mut sock, "text/plain; charset=utf-8", summary.as_bytes()).await
        }
        "/stream" => stream(&mut sock, frames).await,
        _ => {
            sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
            Ok(())
        }
    }
}

async fn respond(sock: &mut TcpStream, kind: &str, body: &[u8]) -> Result<()> {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body).await?;
    Ok(())
}

/// multipart/x-mixed-replace: browsers show each part as the next frame of an <img>.
async fn stream(sock: &mut TcpStream, frames: &Frames) -> Result<()> {
    let mut rx = frames.latest.subscribe();
    sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n").await?;
    loop {
        let frame = rx.borrow_and_update().clone();
        if let Some(jpeg) = frame {
            let head = format!("--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", jpeg.len());
            sock.write_all(head.as_bytes()).await?;
            sock.write_all(&jpeg).await?;
            sock.write_all(b"\r\n").await?;
        }
        rx.changed().await?;
    }
}
