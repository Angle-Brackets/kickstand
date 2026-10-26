// SPDX-License-Identifier: GPL-3.0-only
//! Client against the mock daemon: the protocol, chunking and path rules, end to end over TCP.

use std::sync::Arc;

use crate::client::Client;
use crate::mock;

async fn start(token: &str) -> (Arc<Client>, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("switch-mcp-test-{}-{token}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("atmosphere/crash_reports")).unwrap();
    std::fs::write(root.join("atmosphere/crash_reports/01.log"), "crash!").unwrap();
    std::fs::write(root.join("prod.keys"), "secret").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(mock::serve(listener, root.clone(), "tok".into()));
    (Arc::new(Client::new(addr, token.into())), root)
}

#[tokio::test]
async fn ping_list_read() {
    let (client, _root) = start("tok").await;
    client.ping().await.unwrap();
    let (entries, truncated) = client.list_dir("/atmosphere/crash_reports").await.unwrap();
    assert!(!truncated);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "01.log");
    let (data, size) = client.read("/atmosphere/crash_reports/01.log", 0, 100).await.unwrap();
    assert_eq!((data.as_slice(), size), (&b"crash!"[..], 6));
    let (tail, _) = client.read("/atmosphere/crash_reports/01.log", 3, 100).await.unwrap();
    assert_eq!(tail, b"sh!");
}

#[tokio::test]
async fn upload_round_trip_multi_chunk() {
    let (client, root) = start("tok").await;
    // Not a multiple of the chunk size, so the last chunk is partial.
    let data: Vec<u8> = (0..proto::MAX_CHUNK * 3 + 123).map(|i| (i * 7 % 251) as u8).collect();
    let stats = client.upload("/switch/paperboat/paperboat.nro", &data).await.unwrap();
    assert_eq!(stats.size, data.len() as u64);
    assert_eq!(std::fs::read(root.join("switch/paperboat/paperboat.nro")).unwrap(), data);
    let (back, size) = client.read("/switch/paperboat/paperboat.nro", 0, data.len() as u64).await.unwrap();
    assert_eq!(size, data.len() as u64);
    assert_eq!(back, data);
}

#[tokio::test]
async fn rules_enforced_by_daemon() {
    let (client, _root) = start("tok").await;
    let err = client.read("/prod.keys", 0, 10).await.unwrap_err().to_string();
    assert!(err.contains("Forbidden"), "{err}");
    let err = client.upload("/atmosphere/contents/x", b"x").await.unwrap_err().to_string();
    assert!(err.contains("Forbidden"), "{err}");
    // The connection survives refused requests.
    client.ping().await.unwrap();
}

#[tokio::test]
async fn wrong_token_refused() {
    let (client, _root) = start("nope").await;
    let err = client.ping().await.unwrap_err().to_string();
    assert!(err.contains("Unauthorized"), "{err}");
}
