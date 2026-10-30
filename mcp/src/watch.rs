// SPDX-License-Identifier: GPL-3.0-only
//! `kickstand watch`: a web page on this PC showing the console's screen live (an MJPEG stream of
//! screenshots) and what's running, so a person can follow what an agent does.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::client::Client;

const PAGE: &str = include_str!("watch.html");

/// The newest frame, shared by every viewer; captured only while someone watches.
struct Frames {
    latest: watch::Sender<Option<Arc<Vec<u8>>>>,
}

pub async fn serve(client: Arc<Client>, bind: &str, port: u16, fps: u32) -> Result<()> {
    let listener = TcpListener::bind((bind, port)).await?;
    eprintln!("watching the console at http://{}:{port}/ (Ctrl-C to stop)", if bind == "0.0.0.0" { "<this PC>" } else { bind });
    let (latest, _) = watch::channel(None);
    let frames = Arc::new(Frames { latest });
    tokio::spawn(capture(client.clone(), frames.clone(), fps.clamp(1, 10)));
    loop {
        let (sock, _) = listener.accept().await?;
        let (client, frames) = (client.clone(), frames.clone());
        tokio::spawn(async move {
            let _ = handle(sock, &client, &frames).await;
        });
    }
}

/// Takes screenshots while anyone is subscribed to the stream.
async fn capture(client: Arc<Client>, frames: Arc<Frames>, fps: u32) {
    let period = Duration::from_millis(1000 / u64::from(fps));
    loop {
        if frames.latest.receiver_count() == 0 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        let started = tokio::time::Instant::now();
        match client.screenshot().await {
            Ok(jpeg) => {
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
            let app = match client.app_status().await {
                Ok(Some(app)) => crate::client::describe_app(&app),
                Ok(None) => "home menu (no application running)".into(),
                Err(e) => format!("console not answering: {e:#}"),
            };
            let awake = match client.keep_awake(None).await {
                Ok(0) => "keep-awake off".to_string(),
                Ok(s) => format!("kept awake for {}m more", s / 60),
                Err(_) => String::new(),
            };
            let body = serde_json::json!({ "app": app, "awake": awake }).to_string();
            respond(&mut sock, "application/json", body.as_bytes()).await
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
