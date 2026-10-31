// SPDX-License-Identifier: GPL-3.0-only
//! Client against the mock daemon: the protocol, chunking and path rules, end to end over TCP.

use std::sync::Arc;

use crate::client::Client;
use crate::mock;

async fn start(token: &str) -> (Arc<Client>, std::path::PathBuf) {
    start_with("tok", token).await
}

/// A mock accepting `accepted`, and a client presenting `token`.
async fn start_with(accepted: &str, token: &str) -> (Arc<Client>, std::path::PathBuf) {
    // Tests run in parallel, so each gets its own fake SD card.
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("kickstand-test-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("atmosphere/crash_reports")).unwrap();
    std::fs::write(root.join("atmosphere/crash_reports/01.log"), "crash!").unwrap();
    std::fs::write(root.join("prod.keys"), "secret").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(mock::serve(listener, root.clone(), accepted.into()));
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
    let stats = client.upload("/switch/mygame/mygame.nro", &data).await.unwrap();
    assert_eq!(stats.size, data.len() as u64);
    assert_eq!(std::fs::read(root.join("switch/mygame/mygame.nro")).unwrap(), data);
    let (back, size) = client.read("/switch/mygame/mygame.nro", 0, data.len() as u64).await.unwrap();
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
    // Never at the top of the card, in Atmosphère's folders, or a key file.
    for path in ["/kickstand", "/switch", "/atmosphere/crash_reports/01.log", "/prod.keys"] {
        let err = client.delete(path, true).await.unwrap_err().to_string();
        assert!(err.contains("Forbidden"), "{path}: {err}");
    }
    assert!(root.join("atmosphere/crash_reports/01.log").exists());
}

#[tokio::test]
async fn sysmodule_install_and_status() {
    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    let (client, root) = start_with(TOKEN, TOKEN).await;
    let tokens = root.join(&proto::path::TOKEN_PATH[1..]);
    std::fs::create_dir_all(tokens.parent().unwrap()).unwrap();
    std::fs::write(&tokens, "# paired: 10.0.0.2\nfedcba9876543210fedcba9876543210\n").unwrap();
    let status = client.sysmodule(proto::Request::SysmoduleStatus).await.unwrap();
    assert!(!status.installed && !status.boot2 && status.pid.is_none());
    client.install_sysmodule(b"NSP0").await.unwrap();
    assert_eq!(std::fs::read(root.join(&proto::path::SYSMODULE_NSP[1..])).unwrap(), b"NSP0");
    // Added to the other PC's token, not over it; a second install changes nothing.
    let expected = format!("# paired: 10.0.0.2\nfedcba9876543210fedcba9876543210\n# paired: installed the sysmodule\n{TOKEN}\n");
    assert_eq!(std::fs::read_to_string(&tokens).unwrap(), expected);
    client.install_sysmodule(b"NSP0").await.unwrap();
    assert_eq!(std::fs::read_to_string(&tokens).unwrap(), expected);
    let status = client.sysmodule(proto::Request::SysmoduleStatus).await.unwrap();
    assert!(status.installed && !status.boot2);
    assert!(client.set_boot2(true).await.unwrap().boot2);
    assert_eq!(std::fs::read(root.join(&proto::path::SYSMODULE_BOOT2_FLAG[1..])).unwrap(), b"");
    assert!(!client.set_boot2(false).await.unwrap().boot2);
    // Other programs' contents stay read-only.
    let err = client.upload("/atmosphere/contents/0100000000001000/exefs.nsp", b"x").await.unwrap_err();
    assert!(err.to_string().contains("Forbidden"), "{err}");
}

#[test]
fn hekate_targets() {
    use crate::client::parse_hekate_target;
    use proto::HekateTarget;
    let entries = vec!["Atmosphere PKG3 emuMMC".to_string(), "Stock sysMMC".to_string()];
    assert_eq!(parse_hekate_target("ums", &entries).unwrap(), HekateTarget::Ums);
    assert_eq!(parse_hekate_target("Menu", &entries).unwrap(), HekateTarget::Menu);
    assert_eq!(parse_hekate_target("2", &entries).unwrap(), HekateTarget::Entry(2));
    assert_eq!(parse_hekate_target("atmosphere pkg3 emummc", &entries).unwrap(), HekateTarget::Entry(1));
    assert!(parse_hekate_target("3", &entries).is_err());
    assert!(parse_hekate_target("0", &entries).is_err());
    assert!(parse_hekate_target("Linux", &entries).is_err());
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

#[test]
fn input_steps_parse() {
    let steps = crate::client::parse_steps("r:300 r+a:150 -:500 l=0,1+b:2000 r=-0.5,0:10 touch=640,360:100").unwrap();
    assert_eq!(steps.len(), 6);
    assert_eq!(steps[1].buttons, vec![proto::Button::R, proto::Button::A]);
    assert!(steps[2].buttons.is_empty() && steps[2].ms == 500);
    assert_eq!((steps[3].left, steps[3].buttons.as_slice()), (Some((0, proto::STICK_MAX)), &[proto::Button::B][..]));
    assert_eq!(steps[4].right, Some((-16384, 0)));
    assert_eq!(steps[5].touch, Some((640, 360)));
    for bad in ["a", "a:x", "l=2,0:10", "touch=1280,0:10", "q=1,1:10", "nope:10"] {
        assert!(crate::client::parse_steps(bad).is_err(), "{bad} should be refused");
    }
}

#[test]
fn symbolizer_finds_module_offsets() {
    // Any ELF loads; this test binary will do. Only the address matching is checked here.
    let exe = std::env::current_exe().unwrap();
    let symbols = crate::symbolize::Symbolizer::new(&exe, Some("MyGame")).unwrap();
    assert_eq!(symbols.offsets_in("#3 MyGame+0x1a2b (x) mygame + 0x10"), vec![0x1a2b, 0x10]);
    assert!(symbols.offsets_in("NotMyGame+0x10 MyGame+zz MyGame").is_empty());
    let text = "plain\nframe MyGame+0x40\n";
    let annotated = symbols.annotate(text);
    assert!(annotated.starts_with("plain\nframe MyGame+0x40\n        -> "), "{annotated}");
}

#[test]
fn crash_report_summary_keeps_the_crashed_thread() {
    let report = "Atmosphère Crash Report (v1.7):
Result:                          0x2A8 (2168-0001)

Process Info:
    Process Name:                game
    Program ID:                  0100000000001234
Exception Info:
    Type:                        Data Abort
Crashed Thread Info:
    Thread ID:                   0000000000000002
    Registers:
        X[00]:                   0000000000000005
        LR:                      0000000000801000 (game + 0x1000)
        PC:                      0000000000802000 (game + 0x2000)
Module Info:
    Number of Modules:           01
    Module 00:
        Name:                    game
Thread Report:
    Threads[00]:
        Thread ID:               0000000000000001
        Stack Trace:
            ReturnAddress[00]:   0000000000809999 (game + 0x9999)
    Threads[01]:
        Thread ID:               0000000000000002
        Stack Trace:
            ReturnAddress[00]:   0000000000803000 (game + 0x3000)
            ReturnAddress[01]:   0000000000000000
";
    let summary = crate::crash::summarize_crash(report, None);
    for want in ["Process Name:                game", "Data Abort", "PC:", "(game + 0x2000)", "Stack Trace:", "(game + 0x3000)"] {
        assert!(summary.contains(want), "missing {want:?} in:\n{summary}");
    }
    for unwanted in ["X[00]", "0x9999", "ReturnAddress[01]"] {
        assert!(!summary.contains(unwanted), "{unwanted:?} shouldn't be in:\n{summary}");
    }
}

#[test]
fn fatal_record_decodes() {
    let mut bin = vec![0u8; 0x450];
    bin[..4].copy_from_slice(b"AFE2");
    bin[4..8].copy_from_slice(&0xFFEu32.to_le_bytes());
    bin[8..16].copy_from_slice(&0x0100000000000034u64.to_le_bytes());
    bin[0x10..0x18].copy_from_slice(&0x10801u64.to_le_bytes());
    bin[0x110..0x118].copy_from_slice(&0x1020u64.to_le_bytes());
    bin[0x118..0x120].copy_from_slice(&0x1000u64.to_le_bytes());
    let text = crate::crash::describe_fatal(&bin).unwrap();
    for want in ["std::abort", "0100000000000034 (fatal)", "+ 0x20", "2001-0132", "limit reached"] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
    assert!(crate::crash::describe_fatal(&bin[..0x100]).is_err());
}

/// hbmenu's side of NetLoader (nx-hbmenu `loadnro`): what `nxlink::send` writes must inflate back to the
/// NRO, in pieces hbmenu's 16 KiB buffer takes, with the arguments NUL-separated.
#[tokio::test]
async fn nxlink_send_matches_netloader() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let nro: Vec<u8> = (0..100_000u32).flat_map(|i| (i.wrapping_mul(2_654_435_761) >> 13).to_le_bytes()).collect();
    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let name_len = s.read_u32_le().await.unwrap() as usize;
        let mut name = vec![0; name_len];
        s.read_exact(&mut name).await.unwrap();
        let size = s.read_u32_le().await.unwrap() as usize;
        s.write_i32_le(0).await.unwrap();
        let mut inflate = flate2::Decompress::new(true);
        let mut out = Vec::with_capacity(size);
        while out.len() < size {
            let len = s.read_u32_le().await.unwrap() as usize;
            assert!(len <= 16 * 1024, "piece of {len} bytes is over hbmenu's buffer");
            let mut piece = vec![0; len];
            s.read_exact(&mut piece).await.unwrap();
            inflate.decompress_vec(&piece, &mut out, flate2::FlushDecompress::None).unwrap();
        }
        s.write_i32_le(0).await.unwrap();
        let argv_len = s.read_u32_le().await.unwrap() as usize;
        let mut argv = vec![0; argv_len];
        s.read_exact(&mut argv).await.unwrap();
        (String::from_utf8(name).unwrap(), out, argv)
    });
    let sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    crate::nxlink::send(sock, &nro, "kickstand/x.nro", &["a".into(), "b c".into()]).await.unwrap();
    let (name, received, argv) = server.await.unwrap();
    assert_eq!(name, "kickstand/x.nro");
    assert!(received == nro, "the NRO didn't survive the trip");
    assert_eq!(argv, b"a\0b c\0");
}

#[tokio::test]
async fn memory_addresses_reads_and_writes() {
    use crate::memory::{Elf, Format, Layout, peek, poke, resolve};
    use crate::mock::{MOCK_HEAP, MOCK_NRO};
    let (client, _root) = start("tok").await;
    let layout = Layout::fetch(&client).await.unwrap();
    assert_eq!(layout.nro, Some((MOCK_NRO, 0x2000)));
    let at = |expr: &'static str| {
        let (client, layout) = (client.clone(), &layout);
        async move { resolve(&client, layout, None, Some("Game"), expr).await }
    };
    assert_eq!(at("nro+0x1008").await.unwrap(), MOCK_NRO + 0x1008);
    assert_eq!(at("game + 16").await.unwrap(), MOCK_NRO + 0x10);
    assert_eq!(at("heap+0x20-0x8").await.unwrap(), MOCK_HEAP + 0x18);
    assert_eq!(at("[nro+0x1008]+4").await.unwrap(), MOCK_HEAP + 0x14);
    assert!(at("gPlayer").await.unwrap_err().to_string().contains("ELF"));

    let elf = Elf::load(None, None).unwrap();
    let shown = peek(&client, &elf, "[nro+0x1008]+4", 0, Format::F32).await.unwrap();
    assert!(shown.ends_with(": 1.5\n"), "{shown}");
    let ptr = peek(&client, &elf, "nro+0x1008", 0, Format::Ptr).await.unwrap();
    assert!(ptr.contains(&format!("{:#x}", MOCK_HEAP + 0x10)), "{ptr}");
    let hex = peek(&client, &elf, "nro", 0x20, Format::Hex).await.unwrap();
    assert!(hex.contains("|....") && hex.contains("NRO0"), "{hex}");
    assert!(layout.render(None).contains("<- NRO header"));
    let threads = crate::memory::threads(&client, &elf).await.unwrap();
    assert!(threads.contains("thread 0x5a") && threads.contains("pc  nro+0x100"), "{threads}");
    // lr is the first frame here, so it's shown once; an address outside the NRO stays a number.
    assert!(!threads.contains("lr ") && threads.contains("#0  nro+0x204") && threads.contains("#1  0x1234"), "{threads}");

    let wrote = poke(&client, &elf, "heap+0x14", "-2.25", Format::F32).await.unwrap();
    assert!(wrote.contains("before: 1.5\n") && wrote.contains("after:  -2.25"), "{wrote}");
    poke(&client, &elf, "heap", "0x10000", Format::U16).await.unwrap_err();
    poke(&client, &elf, "heap", "-1", Format::I16).await.unwrap();
    assert!(peek(&client, &elf, "heap", 2, Format::U16).await.unwrap().ends_with(": 65535\n"));
    poke(&client, &elf, "heap+0x40", "hi", Format::Str).await.unwrap();
    assert!(peek(&client, &elf, "heap+0x40", 0, Format::Str).await.unwrap().contains("\"hi\""));
}

/// A client newer than the daemon: a request appended after the daemon's version gets Unsupported, and
/// the same connection keeps working; a client older than MIN_COMPATIBLE_VERSION is refused.
#[tokio::test]
async fn newer_requests_are_refused_politely() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (client, _root) = start("tok").await;
    let addr = client.addr().to_string();
    async fn round_trip(s: &mut tokio::net::TcpStream, body: &[u8]) -> proto::Response {
        s.write_all(&(body.len() as u32).to_le_bytes()).await.unwrap();
        s.write_all(body).await.unwrap();
        let len = s.read_u32_le().await.unwrap() as usize;
        let mut reply = vec![0; len];
        s.read_exact(&mut reply).await.unwrap();
        proto::decode(&reply).unwrap()
    }
    let hello = |version| proto::encode(&proto::Request::Hello { version, token: "tok".into() }).unwrap()[4..].to_vec();
    let mut s = tokio::net::TcpStream::connect(&addr).await.unwrap();
    assert!(matches!(round_trip(&mut s, &hello(proto::PROTOCOL_VERSION + 5)).await, proto::Response::Hello { .. }));
    // Variant 200 doesn't exist (yet).
    match round_trip(&mut s, &[200, 1]).await {
        proto::Response::Error(e) => assert_eq!(e.kind, proto::ErrorKind::Unsupported),
        other => panic!("{other:?}"),
    }
    let ping = proto::encode(&proto::Request::Ping { nonce: 7 }).unwrap()[4..].to_vec();
    assert!(matches!(round_trip(&mut s, &ping).await, proto::Response::Pong { nonce: 7, .. }));
    drop(s); // The mock serves one connection at a time.

    let mut old = tokio::net::TcpStream::connect(&addr).await.unwrap();
    match round_trip(&mut old, &hello(proto::MIN_COMPATIBLE_VERSION - 1)).await {
        proto::Response::Error(e) => assert_eq!(e.kind, proto::ErrorKind::VersionMismatch),
        other => panic!("{other:?}"),
    }
}

/// Games never run in applet mode (the user's rule): the projects in projects.json, by where they deploy
/// to or by file name.
#[test]
fn games_refuse_applet_mode() {
    use crate::launch::is_game;
    let projects: std::collections::BTreeMap<String, crate::project::Project> = serde_json::from_str(
        r#"{"mygame": {"dir": "/src/mygame", "nro": "build/MyGame-debug.nro", "elf": "build/MyGame.elf", "dest": "/switch/mygame/mygame.nro"}}"#,
    )
    .unwrap();
    for game in ["/switch/mygame/mygame.nro", "/Switch/MyGame/MyGame.nro", "/switch/kickstand/mygame.nro", "/switch/kickstand/MyGame-debug.nro"] {
        assert!(is_game(game, &projects), "{game}");
    }
    for tool in ["/switch/kickstand/kickstand.nro", "/switch/mygame/tool.nro", "/switch/NX-Shell/NX-Shell.nro"] {
        assert!(!is_game(tool, &projects), "{tool}");
    }
    assert!(!is_game("/switch/mygame/mygame.nro", &Default::default()));
}

/// A mock console showing pairing code 123456 and answering probes; returns its daemon and probe addresses.
async fn start_pairing() -> (String, std::net::SocketAddr) {
    let root = std::env::temp_dir().join(format!("kickstand-pair-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let probes = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let probe_addr = probes.local_addr().unwrap();
    tokio::spawn(mock::answer_probes(probes, port, true));
    let console = mock::Console::new(root, vec!["tok".into()], Some("123456".into()));
    tokio::spawn(mock::serve_console(listener, Arc::new(console)));
    (format!("127.0.0.1:{port}"), probe_addr)
}

#[tokio::test]
async fn discovers_the_console() {
    let (addr, probe_addr) = start_pairing().await;
    let found = crate::pairing::discover(std::time::Duration::from_millis(300), Some(probe_addr)).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].addr.to_string(), addr);
    assert!(found[0].announce.pairing);
    assert_eq!(found[0].describe(), "Mock Switch (127.0.0.1)");
}

#[tokio::test]
async fn pairs_with_the_code_then_connects_with_the_token() {
    let (addr, _) = start_pairing().await;
    let token = "0123456789abcdef0123456789abcdef";

    let wrong = Client::new(addr.clone(), token.into()).pair("654321").await.unwrap_err();
    assert!(format!("{wrong:#}").contains("wrong pairing code"), "{wrong:#}");
    // Not paired, so Hello with the token fails.
    assert!(Client::new(addr.clone(), token.into()).ping().await.is_err());

    let pairing = Client::new(addr.clone(), token.into());
    pairing.pair("123 456").await.unwrap();
    pairing.ping().await.unwrap();
    drop(pairing);
    Client::new(addr, token.into()).ping().await.unwrap();
}

#[tokio::test]
async fn pairing_needs_the_screen() {
    let (client, _root) = start("tok").await;
    let err = client.pair("123456").await.unwrap_err();
    assert!(format!("{err:#}").contains("isn't pairing"), "{err:#}");
}

#[tokio::test]
async fn reports_what_other_clients_do_without_counting_itself() {
    let (agent, _) = start("tok").await;
    let display = Client::new(agent.addr().to_string(), "tok".into());
    agent.ping().await.unwrap();
    agent.list_dir("/atmosphere/crash_reports").await.unwrap();
    let report = display.activity().await.unwrap().unwrap();
    assert_eq!(report.clients.len(), 2);
    let (you, other): (Vec<_>, Vec<_>) = report.clients.iter().partition(|c| c.you);
    assert_eq!((you[0].requests, you[0].doing.as_str()), (0, "connecting"));
    assert_eq!((other[0].requests, other[0].doing.as_str()), (2, "reading files"));
    let recent: Vec<&str> = report.recent.iter().map(|e| e.what.as_str()).collect();
    assert_eq!(recent, ["reading files"]);
}

#[tokio::test]
async fn another_agent_pressing_shows_after_it_looks() {
    let (agent, _) = start("tok").await;
    let other = Client::new(agent.addr().to_string(), "tok".into());
    other.press(vec![proto::Button::B], 1, 50).await.unwrap();
    other.list_dir("/atmosphere/crash_reports").await.unwrap();
    agent.ping().await.unwrap();
    let agents = agent.agents().await.unwrap().unwrap().agents;
    assert_eq!(crate::client::other_agents(&agents, true), ["an agent on this PC was pressing buttons just now"]);
    assert_eq!(crate::client::other_agents(&agents, false), ["an agent on this PC was pressing buttons just now, then reading files"]);
    // Only looking isn't a clash.
    let looker = Client::new(agent.addr().to_string(), "tok".into());
    looker.list_dir("/").await.unwrap();
    drop(other);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(crate::client::other_agents(&agent.agents().await.unwrap().unwrap().agents, true).is_empty());
}

#[tokio::test]
async fn agents_name_their_app() {
    let (agent, _) = start("tok").await;
    agent.ping().await.unwrap();
    agent.set_app("code").await;
    let other = agent.another();
    other.press(vec![proto::Button::B], 1, 50).await.unwrap();
    let late = Client::new(agent.addr().to_string(), "tok".into());
    late.set_app("antigravity-client").await;
    late.ping().await.unwrap();
    let report = Client::new(agent.addr().to_string(), "tok".into()).agents().await.unwrap().unwrap();
    let apps: Vec<_> = report.agents.iter().filter(|a| !a.you).map(|a| a.app.as_deref()).collect();
    assert_eq!(apps, [Some("code"), Some("code"), Some("antigravity-client")]);
    let ids: Vec<u32> = report.agents.iter().map(|a| a.id).collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "{ids:?}");
    let event = &report.recent[0];
    assert_eq!((event.what.as_str(), event.app.as_deref(), event.id), ("pressing buttons", Some("code"), report.agents[1].id));
}
