// SPDX-License-Identifier: GPL-3.0-only
//! Client against the mock daemon: the protocol, chunking and path rules, end to end over TCP.

use std::sync::Arc;

use crate::client::Client;
use crate::mock;

async fn start(token: &str) -> (Arc<Client>, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("kickstand-test-{}-{token}", std::process::id()));
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
async fn pipelined_read_from_offset() {
    let (client, root) = start("tok").await;
    let data: Vec<u8> = (0..proto::MAX_CHUNK * 9 + 5).map(|i| (i * 13 % 241) as u8).collect();
    std::fs::create_dir_all(root.join("kickstand")).unwrap();
    std::fs::write(root.join("kickstand/big.bin"), &data).unwrap();
    let (back, size) = client.read("/kickstand/big.bin", 0, u64::MAX).await.unwrap();
    assert_eq!((back.len(), size), (data.len(), data.len() as u64));
    assert_eq!(back, data);
    // Starts mid-chunk and stops short of the end.
    let (part, _) = client.read("/kickstand/big.bin", 1000, proto::MAX_CHUNK as u64 * 5).await.unwrap();
    assert_eq!(part, &data[1000..1000 + proto::MAX_CHUNK * 5]);
    // The connection is still in step afterwards.
    client.ping().await.unwrap();
}

#[tokio::test]
async fn delete_files_and_dirs() {
    let (client, root) = start("tok").await;
    client.upload("/kickstand/test/a.bin", b"a").await.unwrap();
    client.upload("/kickstand/test/sub/b.bin", b"b").await.unwrap();
    assert!(!client.delete("/kickstand/test/a.bin", false).await.unwrap());
    assert!(!root.join("kickstand/test/a.bin").exists());
    // Not empty, so a plain delete fails and a recursive one succeeds.
    assert!(client.delete("/kickstand/test", false).await.is_err());
    assert!(client.delete("/kickstand/test", true).await.unwrap());
    assert!(!root.join("kickstand/test").exists());
    // Only inside the write roots, and never a root itself.
    for path in ["/kickstand", "/switch/paperboat", "/atmosphere/crash_reports/01.log", "/prod.keys"] {
        let err = client.delete(path, true).await.unwrap_err().to_string();
        assert!(err.contains("Forbidden"), "{path}: {err}");
    }
    assert!(root.join("atmosphere/crash_reports/01.log").exists());
}

#[tokio::test]
async fn sysmodule_install_and_status() {
    let (client, root) = start("tok").await;
    let status = client.sysmodule(proto::Request::SysmoduleStatus).await.unwrap();
    assert!(!status.installed && !status.boot2 && status.pid.is_none());
    client.install_sysmodule(b"NSP0").await.unwrap();
    assert_eq!(std::fs::read(root.join(&proto::path::SYSMODULE_NSP[1..])).unwrap(), b"NSP0");
    assert_eq!(std::fs::read_to_string(root.join(&proto::path::TOKEN_PATH[1..])).unwrap(), "tok\n");
    let status = client.sysmodule(proto::Request::SysmoduleStatus).await.unwrap();
    assert!(status.installed && !status.boot2);
    // Other programs' contents stay read-only.
    let err = client.upload("/atmosphere/contents/0100000000001000/exefs.nsp", b"x").await.unwrap_err();
    assert!(err.to_string().contains("Forbidden"), "{err}");
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
