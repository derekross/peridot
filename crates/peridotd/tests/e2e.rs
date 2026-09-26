//! Two real daemons, each with its own home folder and in-memory keyring,
//! pairing and syncing through a local relay.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use nostr_sdk::prelude::MockRelay;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

struct Daemon {
    child: Child,
    socket: PathBuf,
    home: PathBuf,
    _dir: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    async fn start(relay: &str, name: &str) -> Self {
        Self::start_with(relay, name, None).await
    }

    /// `opal` is the control socket of an Opal daemon (real or fake).
    async fn start_with(relay: &str, name: &str, opal: Option<&Path>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            format!("relays = [\"{relay}\"]\ndevice_name = \"{name}\"\n"),
        )
        .unwrap();
        Self::start_in(dir, config, opal).await
    }

    /// Start from a config file already written into `dir`.
    async fn start_configured(dir: tempfile::TempDir, config: PathBuf) -> Self {
        Self::start_in(dir, config, None).await
    }

    async fn start_in(dir: tempfile::TempDir, config: PathBuf, opal: Option<&Path>) -> Self {
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let socket = dir.path().join("peridot.sock");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_peridotd"));
        cmd.args(["--memory-keyring", "--socket"]).arg(&socket);
        // Point at a socket that doesn't exist when there's no Opal.
        cmd.arg("--opal-socket").arg(
            opal.map(|p| p.to_path_buf())
                .unwrap_or_else(|| dir.path().join("no-opal.sock")),
        );
        let child = cmd
            .arg("--config")
            .arg(&config)
            .arg("--db")
            .arg(dir.path().join("peridot.db"))
            .arg("--home")
            .arg(&home)
            .env("PERIDOT_LOG", "error")
            .spawn()
            .unwrap();
        for _ in 0..100 {
            if UnixStream::connect(&socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Self {
            child,
            socket,
            home,
            _dir: dir,
        }
    }

    async fn call(&self, method: &str, params: Value) -> Value {
        let mut c = Client::open(&self.socket).await;
        c.call(method, params)
            .await
            .unwrap_or_else(|e| panic!("{method}: {e}"))
    }

    async fn status(&self) -> Value {
        self.call("status", json!(null)).await
    }

    fn write(&self, path: &str, content: &str) {
        let p = self.home.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.home.join(path)).ok()
    }
}

struct Client {
    r: BufReader<tokio::net::unix::OwnedReadHalf>,
    w: tokio::net::unix::OwnedWriteHalf,
    id: u64,
}

impl Client {
    async fn open(socket: &Path) -> Self {
        let (r, w) = UnixStream::connect(socket).await.unwrap().into_split();
        Self {
            r: BufReader::new(r),
            w,
            id: 0,
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.id += 1;
        let line = json!({"id": self.id, "method": method, "params": params}).to_string() + "\n";
        self.w.write_all(line.as_bytes()).await.unwrap();
        loop {
            let mut buf = String::new();
            self.r.read_line(&mut buf).await.unwrap();
            let v: Value = serde_json::from_str(&buf).unwrap();
            if v["id"] == json!(self.id) {
                return match v["error"].as_str() {
                    Some(e) => Err(e.to_string()),
                    None => Ok(v["result"].clone()),
                };
            }
        }
    }
}

/// A local relay for the test. MockRelay picks a port at random, and with
/// several tests starting at once it occasionally lands on a taken one.
async fn mock_relay() -> MockRelay {
    for _ in 0..10 {
        if let Ok(r) = MockRelay::run().await {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no free port for a mock relay");
}

/// Poll until `check` passes (or fail after `secs`).
async fn until(d: &Daemon, secs: u64, what: &str, check: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..secs * 5 {
        let s = d.status().await;
        if check(&s) {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("timed out waiting for {what}: {}", d.status().await);
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_then_sync_a_change() {
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let desk = Daemon::start(&url, "Desk").await;
    let laptop = Daemon::start(&url, "Laptop").await;

    desk.write(
        ".config/hypr/bindings.lua",
        "bind = SUPER, Return, exec, ghostty",
    );
    desk.write(".config/hypr/monitors.lua", "monitor = DP-1, 5120x1440");
    desk.call("setup.start_fresh", json!(null)).await;
    until(&desk, 20, "desk to publish", |s| {
        s["counts"]["in_sync"] == json!(1)
    })
    .await;

    // Pairing: the laptop shows a code, the desk takes it, both show the
    // same number, the desk confirms.
    let offer = laptop.call("pair.new", json!(null)).await;
    let code = offer["code"].as_str().unwrap().to_string();
    assert!(code.starts_with("PDT-"));
    desk.call("pair.join", json!({"code": code})).await;
    let d = until(&desk, 20, "desk to show the number", |s| {
        s["pairing"]["stage"] == json!("confirm")
    })
    .await;
    let l = until(&laptop, 20, "laptop to show the number", |s| {
        s["pairing"]["stage"] == json!("confirm")
    })
    .await;
    assert_eq!(d["pairing"]["number"], l["pairing"]["number"]);
    assert_eq!(d["pairing"]["other"], json!("Laptop"));
    assert_eq!(l["pairing"]["other"], json!("Desk"));
    desk.call("pair.confirm", json!({"matches": true})).await;
    until(&laptop, 20, "laptop to be set up", |s| {
        s["set_up"] == json!(true)
    })
    .await;

    // The desk's bindings arrive as incoming; monitors never do.
    let s = until(&laptop, 20, "incoming bindings", |s| {
        s["counts"]["incoming"] == json!(1)
    })
    .await;
    assert_eq!(s["files"][0]["path"], json!(".config/hypr/bindings.lua"));
    assert_eq!(s["files"][0]["from"], json!("Desk"));
    laptop.call("apply", json!({})).await;
    assert_eq!(
        laptop.read(".config/hypr/bindings.lua").unwrap(),
        "bind = SUPER, Return, exec, ghostty"
    );
    assert_eq!(laptop.read(".config/hypr/monitors.lua"), None);

    // Both computers list each other.
    let s = until(&desk, 20, "desk to see the laptop", |s| {
        s["devices"].as_array().is_some_and(|d| d.len() == 2)
    })
    .await;
    assert!(
        s["devices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["name"] == json!("Laptop"))
    );

    // A change on the laptop (picked up by the file watcher) reaches the desk.
    laptop.write(
        ".config/hypr/bindings.lua",
        "bind = SUPER, Return, exec, alacritty",
    );
    until(&desk, 30, "desk to see the laptop's change", |s| {
        s["counts"]["incoming"] == json!(1)
    })
    .await;
    desk.call("apply", json!({})).await;
    assert_eq!(
        desk.read(".config/hypr/bindings.lua").unwrap(),
        "bind = SUPER, Return, exec, alacritty"
    );

    // And it can be undone.
    let s = desk.status().await;
    let id = s["history"][0]["id"].clone();
    desk.call("history.undo", json!({"id": id})).await;
    assert_eq!(
        desk.read(".config/hypr/bindings.lua").unwrap(),
        "bind = SUPER, Return, exec, ghostty"
    );
    // Kept on the desk only; nothing is waiting there any more.
    let s = desk.status().await;
    assert_eq!(s["files"][0]["status"], json!("kept"));
    assert_eq!(s["counts"]["incoming"], json!(0));

    // Later, with nothing else going on: only the file watcher can notice.
    tokio::time::sleep(Duration::from_secs(2)).await;
    laptop.write(".config/hypr/looknfeel.lua", "general = { gaps_in = 2 }");
    until(&desk, 30, "desk to see a watched change", |s| {
        s["files"].as_array().is_some_and(|f| {
            f.iter().any(|f| {
                f["path"] == json!(".config/hypr/looknfeel.lua") && f["status"] == json!("incoming")
            })
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn subscribers_get_the_state_and_live_events() {
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let d = Daemon::start(&url, "Desk").await;
    let mut c = Client::open(&d.socket).await;
    let first = c.call("subscribe", json!(null)).await.unwrap();
    assert_eq!(first["set_up"], json!(false));
    // A change made elsewhere is pushed to subscribers.
    d.call("setup.start_fresh", json!(null)).await;
    let mut buf = String::new();
    loop {
        buf.clear();
        tokio::time::timeout(Duration::from_secs(10), c.r.read_line(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let v: Value = serde_json::from_str(&buf).unwrap();
        if v["event"] == json!("state") && v["data"]["set_up"] == json!(true) {
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_answer_to_the_number_shares_nothing() {
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let desk = Daemon::start(&url, "Desk").await;
    let laptop = Daemon::start(&url, "Laptop").await;
    desk.call("setup.start_fresh", json!(null)).await;

    let code = laptop.call("pair.new", json!(null)).await["code"]
        .as_str()
        .unwrap()
        .to_string();
    desk.call("pair.join", json!({"code": code})).await;
    until(&desk, 20, "number", |s| {
        s["pairing"]["stage"] == json!("confirm")
    })
    .await;
    desk.call("pair.confirm", json!({"matches": false})).await;
    until(&desk, 10, "cancelled", |s| {
        s["pairing"]["stage"] == json!("cancelled")
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(laptop.status().await["set_up"], json!(false));
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_kit_restores_on_a_new_computer() {
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let old = Daemon::start(&url, "Old").await;
    old.write(".config/kitty/kitty.conf", "font_size 13");
    old.call("setup.start_fresh", json!(null)).await;
    until(&old, 20, "publish", |s| s["counts"]["in_sync"] == json!(1)).await;
    let kit = old.call("recovery.create", json!(null)).await;
    let words = kit["words"].as_str().unwrap();
    assert_eq!(words.split(' ').count(), 6);

    let new = Daemon::start(&url, "New").await;
    let mut c = Client::open(&new.socket).await;
    let wrong = c
        .call(
            "recovery.restore",
            json!({"code": kit["code"], "words": "abacus abacus abacus abacus abacus abacus"}),
        )
        .await;
    assert!(wrong.is_err());
    new.call(
        "recovery.restore",
        json!({"code": kit["code"], "words": words.to_uppercase()}),
    )
    .await;
    until(&new, 20, "incoming after restore", |s| {
        s["counts"]["incoming"] == json!(1)
    })
    .await;
    new.call("apply", json!({})).await;
    assert_eq!(
        new.read(".config/kitty/kitty.conf").unwrap(),
        "font_size 13"
    );
}

/// A stand-in for the Opal daemon: one account, answers the calls Peridot
/// makes (`status`, `app.sign`, `app.nip44`), and can be "locked".
struct FakeOpal {
    socket: PathBuf,
    keys: nostr_sdk::prelude::Keys,
    locked: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _dir: tempfile::TempDir,
}

impl FakeOpal {
    async fn start(label: &str) -> Self {
        use nostr_sdk::prelude::*;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opal.sock");
        let keys = Keys::generate();
        let locked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (k, l, label) = (keys.clone(), locked.clone(), label.to_string());
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let (k, l, label) = (k.clone(), l.clone(), label.clone());
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut lines = BufReader::new(r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Value = serde_json::from_str(&line).unwrap();
                        let id = req["id"].clone();
                        let p = &req["params"];
                        let locked = l.load(std::sync::atomic::Ordering::SeqCst);
                        let result: Result<Value, String> = match req["method"]
                            .as_str()
                            .unwrap_or("")
                        {
                            "status" | "subscribe" => Ok(json!({
                                "has_accounts": true,
                                "accounts": [{"pubkey": k.public_key().to_hex(), "label": label,
                                              "npub": k.public_key().to_bech32().unwrap(), "current": true}],
                            })),
                            "app.sign" if locked => Err("Opal is locked".into()),
                            "app.sign" => {
                                let unsigned: UnsignedEvent =
                                    serde_json::from_value(p["event"].clone()).unwrap();
                                assert_eq!(p["app"], json!("peridot"));
                                assert!(
                                    [30078u16, 5, 21078, 22242, 24242]
                                        .contains(&unsigned.kind.as_u16()),
                                    "kind {}",
                                    unsigned.kind
                                );
                                Ok(json!(k.sign_event(unsigned).unwrap()))
                            }
                            "app.nip44" if locked => Err("Opal is locked".into()),
                            "app.nip44" => {
                                let content = p["content"].as_str().unwrap();
                                let out = match p["op"].as_str().unwrap() {
                                    "encrypt" => nip44::encrypt(
                                        k.secret_key(),
                                        &k.public_key(),
                                        content,
                                        nip44::Version::V2,
                                    )
                                    .unwrap(),
                                    _ => nip44::decrypt(k.secret_key(), &k.public_key(), content)
                                        .unwrap(),
                                };
                                Ok(json!({"content": out}))
                            }
                            m => Err(format!("unknown method: {m}")),
                        };
                        let resp = match result {
                            Ok(v) => json!({"id": id, "result": v}),
                            Err(e) => json!({"id": id, "error": e}),
                        };
                        if w.write_all((resp.to_string() + "\n").as_bytes())
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
            }
        });
        Self {
            socket,
            keys,
            locked,
            _dir: dir,
        }
    }

    fn set_locked(&self, locked: bool) {
        self.locked
            .store(locked, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_opal_identity_syncs_and_pairs_only_with_opal_present() {
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let opal = FakeOpal::start("Derek").await;
    let desk = Daemon::start_with(&url, "Desk", Some(&opal.socket)).await;

    // The welcome screen sees Opal's account; setting up adopts it.
    let s = desk.status().await;
    assert_eq!(s["opal_accounts"][0]["label"], json!("Derek"), "{s}");
    desk.write(
        ".config/hypr/bindings.lua",
        "bind = SUPER, Return, exec, ghostty",
    );
    // A key with nothing on the servers yet can be set up while Opal is
    // locked: the first publishes simply wait for the unlock.
    opal.set_locked(true);
    desk.call("setup.use_opal", json!({})).await;
    let s = until(&desk, 20, "desk waiting for unlock", |s| {
        s["error"]
            .as_str()
            .is_some_and(|e| e.contains("Unlock Opal"))
    })
    .await;
    assert_eq!(s["set_up"], json!(true));
    assert_eq!(s["counts"]["in_sync"], json!(0));
    opal.set_locked(false);
    desk.call("sync.now", json!(null)).await;
    let s = until(&desk, 20, "desk to publish via Opal", |s| {
        s["counts"]["in_sync"] == json!(1)
    })
    .await;
    assert_eq!(s["identity"]["mode"], json!("opal"));
    assert_eq!(s["identity"]["name"], json!("Derek"));
    assert_eq!(
        s["identity"]["pubkey"],
        json!(opal.keys.public_key().to_hex())
    );

    // A computer without Opal can't take an Opal-held identity.
    let bare = Daemon::start(&url, "Bare").await;
    let code = bare.call("pair.new", json!(null)).await["code"]
        .as_str()
        .unwrap()
        .to_string();
    desk.call("pair.join", json!({"code": code})).await;
    until(&desk, 20, "number", |s| {
        s["pairing"]["stage"] == json!("confirm")
    })
    .await;
    desk.call("pair.confirm", json!({"matches": true})).await;
    let s = until(&bare, 20, "bare to fail", |s| {
        s["pairing"]["stage"] == json!("failed")
    })
    .await;
    assert!(
        s["pairing"]["error"].as_str().unwrap().contains("Opal"),
        "{s}"
    );
    assert_eq!(bare.status().await["set_up"], json!(false));

    // One with Opal (same key) pairs and gets the settings.
    let laptop = Daemon::start_with(&url, "Laptop", Some(&opal.socket)).await;
    let code = laptop.call("pair.new", json!(null)).await["code"]
        .as_str()
        .unwrap()
        .to_string();
    desk.call("pair.cancel", json!(null)).await;
    desk.call("pair.join", json!({"code": code})).await;
    until(&desk, 20, "number", |s| {
        s["pairing"]["stage"] == json!("confirm")
    })
    .await;
    desk.call("pair.confirm", json!({"matches": true})).await;
    until(&laptop, 20, "laptop set up", |s| s["set_up"] == json!(true)).await;
    until(&laptop, 20, "incoming", |s| {
        s["counts"]["incoming"] == json!(1)
    })
    .await;
    laptop.call("apply", json!({})).await;
    assert_eq!(
        laptop.read(".config/hypr/bindings.lua").unwrap(),
        "bind = SUPER, Return, exec, ghostty"
    );

    // Locked Opal holds outgoing changes; unlocking lets them through.
    opal.set_locked(true);
    laptop.write(
        ".config/hypr/bindings.lua",
        "bind = SUPER, Return, exec, kitty",
    );
    let s = until(&laptop, 20, "held while locked", |s| {
        s["error"]
            .as_str()
            .is_some_and(|e| e.contains("Unlock Opal"))
    })
    .await;
    assert_eq!(s["counts"]["outgoing"], json!(1));
    opal.set_locked(false);
    laptop.call("sync.now", json!(null)).await;
    until(&laptop, 20, "published after unlock", |s| {
        s["counts"]["outgoing"] == json!(0) && s["error"].is_null()
    })
    .await;
    until(&desk, 20, "desk sees it", |s| {
        s["counts"]["incoming"] == json!(1)
    })
    .await;

    // A key that already has settings on the servers must not be set up
    // while Opal is locked: that would mint a new sync secret over the
    // existing one. Unlocked, it rejoins its settings without pairing.
    let third = Daemon::start_with(&url, "Third", Some(&opal.socket)).await;
    opal.set_locked(true);
    let mut c3 = Client::open(&third.socket).await;
    let err = c3.call("setup.use_opal", json!({})).await.unwrap_err();
    assert!(err.contains("Unlock Opal"), "{err}");
    assert_eq!(third.status().await["set_up"], json!(false));
    opal.set_locked(false);
    third.call("setup.use_opal", json!({})).await;
    until(&third, 20, "third rejoins", |s| {
        s["counts"]["incoming"] == json!(1)
    })
    .await;

    // No recovery kit in Opal mode: Opal's backup is the kit.
    let mut c = Client::open(&laptop.socket).await;
    let err = c.call("recovery.create", json!(null)).await.unwrap_err();
    assert!(err.contains("Opal"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn importing_the_same_key_rejoins_its_settings() {
    use nostr_sdk::prelude::{Keys, ToBech32};
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let keys = Keys::generate();
    let nsec = keys.secret_key().to_bech32().unwrap();

    let a = Daemon::start(&url, "A").await;
    a.write(".config/kitty/kitty.conf", "font_size 13");
    a.call("setup.import", json!({"secret": nsec})).await;
    let s = until(&a, 20, "publish", |s| s["counts"]["in_sync"] == json!(1)).await;
    assert_eq!(s["identity"]["mode"], json!("local"));
    assert_eq!(s["identity"]["pubkey"], json!(keys.public_key().to_hex()));

    // The same key on another computer finds the existing settings (no
    // pairing needed): same sync secret, so the file arrives.
    let b = Daemon::start(&url, "B").await;
    let mut c = Client::open(&b.socket).await;
    assert!(
        c.call("setup.import", json!({"secret": "nsec1notakey"}))
            .await
            .is_err()
    );
    b.call("setup.import", json!({"secret": nsec})).await;
    until(&b, 20, "incoming on B", |s| {
        s["counts"]["incoming"] == json!(1)
    })
    .await;
    b.call("apply", json!({})).await;
    assert_eq!(b.read(".config/kitty/kitty.conf").unwrap(), "font_size 13");
}

/// A stand-in Blossom server: PUT /upload stores the body under its hash
/// (after checking the signed authorization header), GET /<sha> returns
/// it, DELETE /<sha> removes it.
struct FakeBlossom {
    base: String,
    blobs: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
}

impl FakeBlossom {
    async fn start() -> Self {
        use nostr_sdk::prelude::*;
        use sha2::Digest;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let blobs = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
            String,
            Vec<u8>,
        >::new()));
        let store = blobs.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    break;
                };
                let store = store.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 8192];
                    let (head_end, headers) = loop {
                        let n = s.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break (i + 4, String::from_utf8_lossy(&buf[..i]).to_string());
                        }
                    };
                    let mut lines = headers.lines();
                    let request = lines.next().unwrap_or("").to_string();
                    let mut len = 0usize;
                    let mut auth = None;
                    for l in lines {
                        if let Some(v) = l.to_ascii_lowercase().strip_prefix("content-length:") {
                            len = v.trim().parse().unwrap_or(0);
                        }
                        if let Some((k, v)) = l.split_once(':')
                            && k.eq_ignore_ascii_case("authorization")
                        {
                            auth = Some(v.trim().to_string());
                        }
                    }
                    while buf.len() < head_end + len {
                        let n = s.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let body = buf[head_end..(head_end + len).min(buf.len())].to_vec();
                    let mut parts = request.split(' ');
                    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                    let auth_ok = |verb: &str, sha: &str| -> bool {
                        let Some(a) = auth.as_ref().and_then(|a| a.strip_prefix("Nostr ")) else {
                            return false;
                        };
                        let Ok(json) =
                            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, a)
                        else {
                            return false;
                        };
                        let Ok(ev) = Event::from_json(&json) else {
                            return false;
                        };
                        ev.verify().is_ok()
                            && ev.kind.as_u16() == 24242
                            && ev.tags.iter().any(|t| t.as_slice() == ["t", verb])
                            && ev.tags.iter().any(|t| t.as_slice() == ["x", sha])
                    };
                    let (status, resp_body) = match (method, path) {
                        ("PUT", "/upload") => {
                            let sha = hex::encode(sha2::Sha256::digest(&body));
                            if !auth_ok("upload", &sha) {
                                ("401 Unauthorized", String::new())
                            } else {
                                store.lock().unwrap().insert(sha.clone(), body);
                                ("200 OK", format!("{{\"sha256\":\"{sha}\",\"size\":{len}}}"))
                            }
                        }
                        ("GET", p) => {
                            let found = store
                                .lock()
                                .unwrap()
                                .get(p.trim_start_matches('/'))
                                .cloned();
                            match found {
                                Some(b) => {
                                    let mut r = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len()).into_bytes();
                                    r.extend_from_slice(&b);
                                    let _ = tokio::io::AsyncWriteExt::write_all(&mut s, &r).await;
                                    return;
                                }
                                None => ("404 Not Found", String::new()),
                            }
                        }
                        ("DELETE", p) => {
                            let sha = p.trim_start_matches('/').to_string();
                            if !auth_ok("delete", &sha) {
                                ("401 Unauthorized", String::new())
                            } else {
                                store.lock().unwrap().remove(&sha);
                                ("200 OK", String::new())
                            }
                        }
                        _ => ("404 Not Found", String::new()),
                    };
                    let r = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                        resp_body.len()
                    );
                    let _ = tokio::io::AsyncWriteExt::write_all(&mut s, r.as_bytes()).await;
                });
            }
        });
        Self { base, blobs }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn private_links_upload_open_and_revoke() {
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let blossom = FakeBlossom::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "relays = [\"{url}\"]\ndevice_name = \"Desk\"\n[share]\nservers = [\"{}\"]\nviewer = \"https://myperidot.app/s\"\nexpire_days = 3\n",
            blossom.base
        ),
    )
    .unwrap();
    let d = Daemon::start_configured(dir, config).await;
    d.call("setup.start_fresh", json!(null)).await;
    until(&d, 20, "set up", |s| s["set_up"] == json!(true)).await;

    // Share a file: the blob on the server is not the file, and the link
    // (with its key) opens it.
    d.write("Pictures/shot.png", "PNG data that is not really a PNG");
    let share = d
        .call(
            "share.file",
            json!({"path": d.home.join("Pictures/shot.png")}),
        )
        .await;
    let link_url = share["url"].as_str().unwrap().to_string();
    assert!(
        link_url.starts_with("https://myperidot.app/s#1."),
        "{link_url}"
    );
    let link = peridot_sync::share::Link::parse(&link_url).unwrap();
    let stored = blossom
        .blobs
        .lock()
        .unwrap()
        .get(&link.sha256)
        .cloned()
        .unwrap();
    assert!(!stored.windows(8).any(|w| w == b"PNG data"));
    let (header, data) = peridot_sync::share::open(&link.key, &stored).unwrap();
    assert_eq!(header.name, "shot.png");
    assert_eq!(header.mime, "image/png");
    assert_eq!(data, b"PNG data that is not really a PNG");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expires = share["expires"].as_u64().unwrap();
    assert!(expires > now + 2 * 86400 && expires <= now + 3 * 86400 + 5);

    // Clipboard text works the same way; the panel lists both.
    d.call("share.text", json!({"text": "hello from the clipboard"}))
        .await;
    let s = d.status().await;
    assert_eq!(s["shares"].as_array().unwrap().len(), 2);

    // Revoking removes the blob from the server.
    let id = share["id"].as_i64().unwrap();
    d.call("share.revoke", json!({"id": id})).await;
    assert!(blossom.blobs.lock().unwrap().get(&link.sha256).is_none());
    let s = d.status().await;
    assert_eq!(s["shares"].as_array().unwrap().len(), 1);
}
