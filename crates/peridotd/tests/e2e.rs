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
        // Sync-only daemons keep the Gallery off: no catalogue downloads,
        // no public relays, in a test.
        std::fs::write(
            &config,
            format!(
                "relays = [\"{relay}\"]\ndevice_name = \"{name}\"\n[gallery]\nenabled = false\n"
            ),
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
        // The test binary plays the panel (dangerous methods need it).
        if std::env::var_os("PERIDOT_TEST_UNTRUSTED").is_none() {
            cmd.arg("--panel-exe").arg(std::env::current_exe().unwrap());
        }
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
            .env(
                "PERIDOT_LOG",
                std::env::var("PERIDOT_TEST_LOG").unwrap_or_else(|_| "error".into()),
            )
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
    // Both sides say yes; nothing moves until they have.
    desk.call("pair.confirm", json!({"confirm": true, "matches": true}))
        .await;
    until(&desk, 10, "desk waiting for the laptop", |s| {
        s["pairing"]["stage"] == json!("waiting_other")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(laptop.status().await["set_up"], json!(false));
    laptop
        .call("pair.confirm", json!({"confirm": true, "matches": true}))
        .await;
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
    desk.call("pair.confirm", json!({"confirm": true, "matches": false}))
        .await;
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
    let kit = old
        .call("recovery.create", json!({"confirm": true, "confirm": true}))
        .await;
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

/// How the fake Opal answers prompts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    Allow,
    /// Everything is refused, pairing included.
    Deny,
    /// Only what Opal treats as sensitive is refused (Blossom, relay auth,
    /// decrypt); syncing goes through.
    DenySensitive,
}

/// A stand-in for the Opal daemon with the local-app protocol: one account,
/// pairing that hands out a token, `app.sign`/`app.nip44`/`app.status` that
/// need it, a lock, and a choice of answers.
/// What Peridot declares to Opal when pairing.
const DECLARED: [u16; 12] = [30078, 22242, 24242, 0, 3, 5, 7, 13, 17, 1111, 1985, 30490];

struct FakeOpal {
    socket: PathBuf,
    keys: nostr_sdk::prelude::Keys,
    /// Every account, the first being `keys`. Added ones come from
    /// `add_account` (an import in Opal's Profiles).
    accounts: std::sync::Arc<std::sync::Mutex<Vec<nostr_sdk::prelude::Keys>>>,
    locked: std::sync::Arc<std::sync::atomic::AtomicBool>,
    answer: std::sync::Arc<std::sync::Mutex<Answer>>,
    token: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    connects: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    signs: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    _dir: tempfile::TempDir,
}

impl FakeOpal {
    async fn start(label: &str) -> Self {
        use nostr_sdk::prelude::*;
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opal.sock");
        let keys = Keys::generate();
        let locked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let answer = std::sync::Arc::new(std::sync::Mutex::new(Answer::Allow));
        let token = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let connects = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let signs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let accounts = std::sync::Arc::new(std::sync::Mutex::new(vec![keys.clone()]));
        let paired_key = std::sync::Arc::new(std::sync::Mutex::new(None::<Keys>));
        // How many kinds the last pairing declared (3 = sync only).
        let declared_kinds = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let shared = (
            keys.clone(),
            locked.clone(),
            label.to_string(),
            answer.clone(),
            token.clone(),
            connects.clone(),
            signs.clone(),
            accounts.clone(),
            paired_key.clone(),
            declared_kinds.clone(),
        );
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let (
                    k0,
                    l,
                    label,
                    answer,
                    token,
                    connects,
                    signs,
                    accounts,
                    paired_key,
                    declared_kinds,
                ) = shared.clone();
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut lines = BufReader::new(r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Value = serde_json::from_str(&line).unwrap();
                        let id = req["id"].clone();
                        let p = &req["params"];
                        let locked = l.load(Ordering::SeqCst);
                        let answer = *answer.lock().unwrap();
                        let paired = p["token"]
                            .as_str()
                            .is_some_and(|t| token.lock().unwrap().as_deref() == Some(t));
                        // The key the token signs for (the first account
                        // until a pairing names another).
                        let k = paired_key
                            .lock()
                            .unwrap()
                            .clone()
                            .unwrap_or_else(|| k0.clone());
                        let all: Vec<Keys> = accounts.lock().unwrap().clone();
                        let sensitive_no = |sensitive: bool| {
                            answer == Answer::Deny || (answer == Answer::DenySensitive && sensitive)
                        };
                        let result: Result<Value, String> = match req["method"]
                            .as_str()
                            .unwrap_or("")
                        {
                            "status" | "subscribe" => Ok(json!({
                                "has_accounts": true,
                                "accounts": all.iter().enumerate().map(|(i, a)| json!({
                                    "pubkey": a.public_key().to_hex(),
                                    "label": if i == 0 { label.clone() } else { format!("{label} {i}") },
                                    "npub": a.public_key().to_bech32().unwrap(),
                                    "current": i == 0,
                                })).collect::<Vec<_>>(),
                            })),
                            "app.connect" => {
                                assert_eq!(p["app"], json!("peridot"));
                                assert_eq!(p["name"], json!("Peridot"));
                                let declared: Vec<u16> =
                                    serde_json::from_value(p["kinds"].clone()).unwrap();
                                assert!(
                                    declared == DECLARED.to_vec()
                                        || declared == vec![30078u16, 22242, 24242],
                                    "{declared:?}"
                                );
                                assert_eq!(p["nip44"], json!(true));
                                assert_eq!(p["dm"], json!(declared.len() > 3));
                                // Which account: the named one, else the first.
                                let chosen = match p["pubkey"].as_str() {
                                    Some(pk) => all
                                        .iter()
                                        .find(|a| a.public_key().to_hex() == pk)
                                        .cloned()
                                        .unwrap_or_else(|| panic!("unknown account {pk}")),
                                    None => k0.clone(),
                                };
                                if answer == Answer::Deny {
                                    Err("declined".into())
                                } else {
                                    // Pairing works while locked (it needs no key).
                                    let t = Keys::generate().secret_key().to_secret_hex();
                                    *token.lock().unwrap() = Some(t.clone());
                                    *paired_key.lock().unwrap() = Some(chosen.clone());
                                    *declared_kinds.lock().unwrap() = declared.len();
                                    connects.fetch_add(1, Ordering::SeqCst);
                                    Ok(json!({"token": t, "pubkey": chosen.public_key().to_hex()}))
                                }
                            }
                            "app.status" if !paired => Err("not paired".into()),
                            "app.status" => Ok(json!({
                                "pubkey": k.public_key().to_hex(), "name": "Peridot", "policy": "basic"
                            })),
                            "app.sign" | "app.nip44" if !paired => Err("not paired".into()),
                            "app.sign" | "app.nip44" if locked => Err("Opal is locked".into()),
                            "app.sign" => {
                                let unsigned: UnsignedEvent =
                                    serde_json::from_value(p["event"].clone()).unwrap();
                                let kind = unsigned.kind.as_u16();
                                let allowed = *declared_kinds.lock().unwrap();
                                if !DECLARED.contains(&kind)
                                    || (allowed == 3 && ![30078u16, 22242, 24242].contains(&kind))
                                {
                                    Err(format!(
                                        "Peridot didn't declare kind {kind} when it paired"
                                    ))
                                } else if sensitive_no(kind != 30078) {
                                    Err("user rejected".into())
                                } else {
                                    signs.fetch_add(1, Ordering::SeqCst);
                                    Ok(json!(k.sign_event(unsigned).unwrap()))
                                }
                            }
                            "app.nip44" => {
                                let content = p["content"].as_str().unwrap();
                                let op = p["op"].as_str().unwrap();
                                // The newer Opal: encrypt to someone else, for
                                // private messages (never decrypt).
                                let peer = p["pubkey"]
                                    .as_str()
                                    .map(|h| PublicKey::from_hex(h).unwrap())
                                    .unwrap_or(k.public_key());
                                if op == "decrypt" && peer != k.public_key() {
                                    Err("local apps only decrypt their own data".into())
                                } else if sensitive_no(op == "decrypt") {
                                    Err("user rejected".into())
                                } else {
                                    let out = match op {
                                        "encrypt" => nip44::encrypt(
                                            k.secret_key(),
                                            &peer,
                                            content,
                                            nip44::Version::V2,
                                        )
                                        .unwrap(),
                                        _ => {
                                            nip44::decrypt(k.secret_key(), &k.public_key(), content)
                                                .unwrap()
                                        }
                                    };
                                    Ok(json!({"content": out}))
                                }
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
            answer,
            token,
            connects,
            signs,
            accounts,
            _dir: dir,
        }
    }

    /// What "Add account" in Opal's Profiles does with a Peridot code:
    /// open the ncryptsec with its password and keep the key.
    fn add_account(&self, code: &str, words: &str) -> nostr_sdk::prelude::Keys {
        let keys = peridot_sync::recovery::open_kit(code, words).unwrap();
        self.accounts.lock().unwrap().push(keys.clone());
        keys
    }

    fn set_locked(&self, locked: bool) {
        self.locked
            .store(locked, std::sync::atomic::Ordering::SeqCst);
    }

    fn set_answer(&self, a: Answer) {
        *self.answer.lock().unwrap() = a;
    }

    /// Revoke Peridot, as the Apps view would.
    fn revoke(&self) {
        *self.token.lock().unwrap() = None;
    }

    fn connects(&self) -> usize {
        self.connects.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn signs(&self) -> usize {
        self.signs.load(std::sync::atomic::Ordering::SeqCst)
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
    // locked: pairing needs no key, and the first publishes simply wait
    // for the unlock.
    opal.set_locked(true);
    desk.call("setup.use_opal", json!({})).await;
    let s = until(&desk, 20, "desk waiting for unlock", |s| {
        s["error"]
            .as_str()
            .is_some_and(|e| e.contains("Unlock Opal"))
    })
    .await;
    assert_eq!(s["set_up"], json!(true));
    // Settings sync regardless (the epoch's own key signs them); only the
    // root event, which the identity signs, waits for the unlock.
    assert_eq!(s["counts"]["in_sync"], json!(1));
    assert_eq!(opal.connects(), 1, "paired once during setup");
    assert_eq!(s["opal"]["paired"], json!(true), "{s}");
    assert_eq!(s["opal"]["needs_pairing"], json!(false));
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
    until(&bare, 20, "number", |s| {
        s["pairing"]["stage"] == json!("confirm")
    })
    .await;
    desk.call("pair.confirm", json!({"confirm": true, "matches": true}))
        .await;
    bare.call("pair.confirm", json!({"confirm": true, "matches": true}))
        .await;
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
    until(&laptop, 20, "number", |s| {
        s["pairing"]["stage"] == json!("confirm")
    })
    .await;
    desk.call("pair.confirm", json!({"confirm": true, "matches": true}))
        .await;
    laptop
        .call("pair.confirm", json!({"confirm": true, "matches": true}))
        .await;
    until(&laptop, 20, "laptop set up", |s| s["set_up"] == json!(true)).await;
    assert_eq!(opal.connects(), 2, "the laptop paired with Opal too");
    until(&laptop, 20, "incoming", |s| {
        s["counts"]["incoming"] == json!(1)
    })
    .await;
    laptop.call("apply", json!({})).await;
    assert_eq!(
        laptop.read(".config/hypr/bindings.lua").unwrap(),
        "bind = SUPER, Return, exec, ghostty"
    );

    // A locked Opal doesn't hold settings up any more: the epoch's own
    // key signs them. Only the identity's own events wait for the unlock.
    opal.set_locked(true);
    let signs = opal.signs();
    laptop.write(
        ".config/hypr/bindings.lua",
        "bind = SUPER, Return, exec, kitty",
    );
    until(&laptop, 20, "published while locked", |s| {
        s["counts"]["outgoing"] == json!(0) && s["counts"]["in_sync"] == json!(1)
    })
    .await;
    assert_eq!(opal.signs(), signs, "Opal wasn't asked for a setting");
    opal.set_locked(false);
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
    // The locked attempt paired (pairing needs no key); the second attempt
    // found that pairing still good and didn't ask again.
    assert_eq!(opal.connects(), 3);

    // No recovery kit in Opal mode: Opal's backup is the kit.
    let mut c = Client::open(&laptop.socket).await;
    let err = c
        .call("recovery.create", json!({"confirm": true, "confirm": true}))
        .await
        .unwrap_err();
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

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_opal_pairing_is_retried_once_then_asks_you() {
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let opal = FakeOpal::start("Derek").await;
    let desk = Daemon::start_with(&url, "Desk", Some(&opal.socket)).await;
    desk.call("setup.use_opal", json!({})).await;
    desk.write(".config/kitty/kitty.conf", "font_size 13");
    until(&desk, 20, "first sync", |s| {
        s["counts"]["in_sync"] == json!(1)
    })
    .await;
    assert_eq!(opal.connects(), 1);

    // Revoked in Opal's Apps view: the next signature fails, Peridot pairs
    // again by itself (a prompt in Opal's bar), and the change goes out.
    opal.revoke();
    // Settings sign themselves; the root event is what needs Opal. A
    // rotation publishes it again, and finds the pairing gone.
    let mut c = crate::Client::open(&desk.socket).await;
    let _ = c.call("sync.rotate", json!(null)).await;
    for _ in 0..150 {
        if opal.connects() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let s = until(&desk, 30, "re-paired and published", |s| {
        s["counts"]["outgoing"] == json!(0)
            && s["counts"]["in_sync"] == json!(1)
            && s["error"].is_null()
    })
    .await;
    assert_eq!(opal.connects(), 2, "{s}");
    assert_eq!(s["opal"]["paired"], json!(true));

    // Revoked again, and this time the prompt is declined: one automatic
    // try, then it's up to you.
    opal.set_answer(Answer::Deny);
    opal.revoke();
    let _ = c.call("sync.rotate", json!(null)).await;
    let s = until(&desk, 30, "needs pairing", |s| {
        s["opal"]["needs_pairing"] == json!(true)
            && s["error"]
                .as_str()
                .is_some_and(|e| e.contains("Pair Peridot with Opal"))
    })
    .await;
    assert_eq!(opal.connects(), 2, "{s}");
    assert!(
        s["opal"]["pair_error"]
            .as_str()
            .is_some_and(|e| e.contains("declined")),
        "{s}"
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(opal.connects(), 2, "no retry loop");

    opal.set_answer(Answer::Allow);
    desk.call("opal.pair", json!(null)).await;
    let s = until(&desk, 30, "published after pairing", |s| {
        s["counts"]["outgoing"] == json!(0) && s["error"].is_null()
    })
    .await;
    assert_eq!(opal.connects(), 3);
    assert_eq!(s["opal"]["needs_pairing"], json!(false));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_opal_signature_keeps_the_change_and_stops_asking() {
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let opal = FakeOpal::start("Derek").await;
    let desk = Daemon::start_with(&url, "Desk", Some(&opal.socket)).await;
    desk.call("setup.use_opal", json!({})).await;
    until(&desk, 20, "set up", |s| s["set_up"] == json!(true)).await;

    opal.set_answer(Answer::Deny);
    desk.write(".config/kitty/kitty.conf", "font_size 13");
    // The change itself syncs (the epoch's key signs it); the root event,
    // which Opal signs, is refused and held.
    let s = until(&desk, 20, "held after a no", |s| {
        s["counts"]["in_sync"] == json!(1)
            && s["error"]
                .as_str()
                .is_some_and(|e| e.contains("didn't allow") || e.contains("ask again"))
    })
    .await;
    assert!(s["opal"]["held_until"].is_u64(), "{s}");
    let asked = opal.signs();
    // Another change while held: no new prompt in Opal.
    desk.write(".config/kitty/kitty.conf", "font_size 14");
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(opal.signs(), asked);

    // "Sync now" asks again; with a yes the change goes out.
    opal.set_answer(Answer::Allow);
    desk.call("sync.now", json!(null)).await;
    let s = until(&desk, 20, "published", |s| {
        s["counts"]["outgoing"] == json!(0)
            && s["counts"]["in_sync"] == json!(1)
            && s["error"].is_null()
    })
    .await;
    assert!(s["opal"]["held_until"].is_null(), "{s}");
    assert_eq!(
        desk.read(".config/kitty/kitty.conf").unwrap(),
        "font_size 14"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn private_links_go_through_opal() {
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let opal = FakeOpal::start("Derek").await;
    let blossom = FakeBlossom::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "relays = [\"{url}\"]\ndevice_name = \"Desk\"\n[share]\nservers = [\"{}\"]\nviewer = \"https://myperidot.app/s\"\n",
            blossom.base
        ),
    )
    .unwrap();
    let d = Daemon::start_in(dir, config, Some(&opal.socket)).await;
    d.call("setup.use_opal", json!({})).await;
    until(&d, 20, "set up", |s| s["set_up"] == json!(true)).await;

    let signs = opal.signs();
    let share = d
        .call("share.text", json!({"text": "hello from Opal"}))
        .await;
    assert!(
        share["url"]
            .as_str()
            .unwrap()
            .starts_with("https://myperidot.app/s#")
    );
    assert!(opal.signs() > signs, "the Blossom auth was signed by Opal");

    // Opal refuses the (sensitive) upload authorization.
    opal.set_answer(Answer::DenySensitive);
    let mut c = Client::open(&d.socket).await;
    let err = c
        .call("share.text", json!({"text": "again"}))
        .await
        .unwrap_err();
    assert!(err.contains("Opal didn't allow the upload"), "{err}");
    // Syncing is unaffected by a refused upload.
    d.write(".config/kitty/kitty.conf", "font_size 13");
    until(&d, 20, "sync still works", |s| {
        s["counts"]["in_sync"] == json!(1)
    })
    .await;
}

// ── The Gallery and private messages ────────────────────────────────

/// A stand-in for the catalogue sites: GET /plugins and GET /themes.
struct FakeSite {
    base: String,
}

impl FakeSite {
    async fn start(plugins: &'static str, themes: &'static str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    loop {
                        let n = s.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).to_string();
                    let path = head.split(' ').nth(1).unwrap_or("");
                    let body = match path {
                        "/plugins" => plugins,
                        "/themes" => themes,
                        _ => "",
                    };
                    let status = if body.is_empty() {
                        "404 Not Found"
                    } else {
                        "200 OK"
                    };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        Self { base }
    }
}

const PLUGINS_JSON: &str = r#"{"plugins":[
  {"id":"derekross.calendar","name":"Calendar Clock","description":"A clock with your agenda","author":"derekross",
   "category":"Widgets","tags":["bar","time"],"repo":"https://github.com/derekross/omarchy-calendar","stars":42,"installAvailable":true},
  {"id":"x.other","name":"Other Thing","description":"Something else","author":"x",
   "category":"System","tags":[],"repo":"https://github.com/x/other","stars":1,"installAvailable":true}
]}"#;
const THEMES_JSON: &str = r#"[
  {"name":"Rose Pine","github_url":"https://github.com/rose/omarchy-rose-pine-theme","github_owner":"rose",
   "description":"Soho vibes","primary_hue":"purple","is_builtin":0,"stars":40}
]"#;

/// Config for a daemon whose gallery reads the fake catalogues and the
/// mock relay.
fn gallery_config(relay: &str, name: &str, site: &FakeSite) -> String {
    format!(
        "relays = [\"{relay}\"]\ndevice_name = \"{name}\"\n[gallery]\nrelays = [\"{relay}\"]\nplugins_url = \"{0}/plugins\"\nthemes_url = \"{0}/themes\"\n",
        site.base
    )
}

async fn gallery_daemon(relay: &str, name: &str, site: &FakeSite, opal: Option<&Path>) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, gallery_config(relay, name, site)).unwrap();
    Daemon::start_in(dir, config, opal).await
}

#[tokio::test(flavor = "multi_thread")]
async fn likes_reviews_and_setups_reach_other_users_ranked_by_trust() {
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let site = FakeSite::start(PLUGINS_JSON, THEMES_JSON).await;
    let desk = gallery_daemon(&url, "Desk", &site, None).await;
    let laptop = gallery_daemon(&url, "Laptop", &site, None).await;
    desk.call("setup.start_fresh", json!(null)).await;
    laptop.call("setup.start_fresh", json!(null)).await;
    for d in [&desk, &laptop] {
        until(d, 30, "the catalogues", |s| {
            s["gallery"]["plugins"] == json!(2) && s["gallery"]["themes"] == json!(1)
        })
        .await;
    }
    let clock = "https://github.com/derekross/omarchy-calendar";

    // Names: a fresh key has none until you pick one.
    let p = desk.call("profile.get", json!(null)).await;
    assert!(p["profile"].is_null(), "{p}");
    let p = desk.call("profile.set", json!({"name": "Derek"})).await;
    assert_eq!(p["name"], json!("Derek"));
    let mut c = Client::open(&desk.socket).await;
    let again = c.call("profile.set", json!({"name": "Someone else"})).await;
    assert!(again.unwrap_err().contains("already has a name"));

    // The desk likes and reviews the clock, and publishes its setup.
    desk.call(
        "gallery.like",
        json!({"url": format!("{clock}.git"), "on": true}),
    )
    .await;
    let r = desk
        .call(
            "gallery.review",
            json!({"url": clock, "text": "Lovely", "rating": 5}),
        )
        .await;
    assert_eq!(r["author"], json!("Derek"));
    desk.write(".local/state/omarchy/current/theme.name", "rose-pine\n");
    desk.write(
        ".config/omarchy/themes/rose-pine/.git/config",
        "[remote \"origin\"]\n\turl = https://github.com/rose/omarchy-rose-pine-theme.git\n",
    );
    // A linked checkout (one you develop) with a public origin, a folder
    // with no git at all whose id the catalogue knows, and one it doesn't.
    let dev = desk.home.join("dev-calendar");
    std::fs::create_dir_all(dev.join(".git")).unwrap();
    std::fs::write(
        dev.join(".git/config"),
        "[remote \"origin\"]\n\turl = git@github.com:derekross/omarchy-calendar.git\n",
    )
    .unwrap();
    std::fs::create_dir_all(desk.home.join(".config/omarchy/plugins")).unwrap();
    std::os::unix::fs::symlink(&dev, desk.home.join(".config/omarchy/plugins/calendar")).unwrap();
    desk.write(".config/omarchy/plugins/x.other/manifest.json", "{}");
    desk.write(".config/omarchy/plugins/nobody.knows/manifest.json", "{}");
    let mine = desk.call("gallery.setup.mine", json!(null)).await;
    assert_eq!(mine["can_publish"], json!(true), "{mine}");
    assert_eq!(mine["theme"], json!("rose-pine"));
    let cands = mine["candidates"].as_array().cloned().unwrap_or_default();
    let how = |name: &str| {
        cands
            .iter()
            .find(|c| c["name"] == json!(name))
            .map(|c| {
                (
                    c["how"].as_str().unwrap_or("").to_string(),
                    c["url"].clone(),
                )
            })
            .unwrap_or_else(|| panic!("no candidate {name}: {mine}"))
    };
    assert_eq!(
        how("rose-pine"),
        (
            "git".into(),
            json!("https://github.com/rose/omarchy-rose-pine-theme")
        )
    );
    assert_eq!(how("calendar"), ("linked".into(), json!(clock)));
    assert_eq!(
        how("x.other"),
        ("catalogue".into(), json!("https://github.com/x/other"))
    );
    assert_eq!(how("nobody.knows"), ("unknown".into(), Value::Null));
    // Publish with the catalogue one left out.
    let setup = desk
        .call(
            "gallery.setup.publish",
            json!({"title": "Derek's desk", "summary": "Rose Pine and a clock",
                   "include": ["https://github.com/rose/omarchy-rose-pine-theme", format!("{clock}.git")]}),
        )
        .await;
    assert_eq!(
        setup["themes"],
        json!(["https://github.com/rose/omarchy-rose-pine-theme"])
    );
    assert_eq!(setup["plugins"], json!([clock]));
    // Without a selection, everything with a source goes in.
    let all = desk
        .call(
            "gallery.setup.publish",
            json!({"title": "Everything", "without_theme": true}),
        )
        .await;
    assert_eq!(all["plugins"].as_array().map(Vec::len), Some(2));
    assert!(all["theme"].is_null());
    desk.call(
        "gallery.setup.remove",
        json!({"coordinate": all["coordinate"]}),
    )
    .await;
    let coordinate = setup["coordinate"].as_str().unwrap().to_string();

    // The laptop sees all of it, by name.
    let desk_pubkey = desk.status().await["identity"]["pubkey"]
        .as_str()
        .unwrap()
        .to_string();
    let mut seen = None;
    for _ in 0..100 {
        let v = laptop
            .call("gallery.list", json!({"kind": "plugin", "query": "clock"}))
            .await;
        let items = v["items"].as_array().cloned().unwrap_or_default();
        if items.len() == 1 && items[0]["likes"] == json!(1) && items[0]["reviews"] == json!(1) {
            seen = Some(items[0].clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let item = seen.expect("the laptop sees the like and the review");
    assert_eq!(item["score"], json!(1), "a stranger's like counts once");
    assert_eq!(item["rating"], json!(5.0));
    assert_eq!(item["liked"], json!(false));
    assert_eq!(item["liked_by"], json!([]));
    let reviews = laptop.call("gallery.reviews", json!({"url": clock})).await;
    assert_eq!(reviews[0]["author"], json!("Derek"));
    assert_eq!(reviews[0]["text"], json!("Lovely"));
    let setups = laptop.call("gallery.setups", json!({})).await;
    assert_eq!(setups[0]["coordinate"], json!(coordinate));
    assert_eq!(setups[0]["author"], json!("Derek"));
    assert_eq!(setups[0]["likes"], json!(0));
    laptop
        .call(
            "gallery.setup.like",
            json!({"coordinate": coordinate, "on": true}),
        )
        .await;
    let setups = laptop.call("gallery.setups", json!({})).await;
    assert_eq!(
        (setups[0]["likes"].clone(), setups[0]["liked"].clone()),
        (json!(1), json!(true))
    );
    let mut liked = false;
    for _ in 0..100 {
        let v = desk.call("gallery.setups", json!({})).await;
        if v[0]["likes"] == json!(1) {
            liked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(liked, "the desk sees the like on its setup");
    assert_eq!(setups[0]["installed"], json!(0));
    assert_eq!(setups[0]["total"], json!(2));
    // Top of the list: the liked one first, then by stars.
    let v = laptop.call("gallery.list", json!({"kind": "plugin"})).await;
    assert_eq!(v["items"][0]["name"], json!("Calendar Clock"));
    assert_eq!(v["total"], json!(2));

    // Following the desk makes its like weigh more, and shows its name.
    laptop
        .call("gallery.follow", json!({"pubkey": desk_pubkey, "on": true}))
        .await;
    let v = laptop
        .call("gallery.list", json!({"kind": "plugin", "query": "clock"}))
        .await;
    assert_eq!(v["items"][0]["score"], json!(4), "{v}");
    assert_eq!(v["items"][0]["liked_by"], json!(["Derek"]));
    let s = laptop.status().await;
    assert_eq!(s["gallery"]["following"], json!(1), "{s}");
    let following = laptop.call("gallery.following", json!(null)).await;
    assert_eq!(following[0]["name"], json!("Derek"));

    // Taking the like back reaches the laptop too.
    desk.call("gallery.like", json!({"url": clock, "on": false}))
        .await;
    let mut gone = false;
    for _ in 0..100 {
        let v = laptop
            .call("gallery.list", json!({"kind": "plugin", "query": "clock"}))
            .await;
        if v["items"][0]["likes"] == json!(0) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(gone, "the like was taken back");

    // A repository nobody listed yet can be put on the map.
    let listed = laptop
        .call(
            "gallery.list_item",
            json!({"url": "https://github.com/laptop/omarchy-mine-theme", "kind": "theme", "name": "Mine"}),
        )
        .await;
    assert_eq!(listed["source"], json!("nostr"));
    let mut found = false;
    for _ in 0..100 {
        let v = desk
            .call("gallery.list", json!({"kind": "theme", "query": "mine"}))
            .await;
        if v["total"] == json!(1) {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(found, "the desk sees the new listing");
}

/// Read the private messages waiting for `keys` on the relay.
async fn inbox(
    relay: &str,
    keys: &nostr_sdk::prelude::Keys,
) -> Vec<nostr_sdk::prelude::UnwrappedGift> {
    use nostr_sdk::prelude::*;
    let client = nostr_sdk::client::Client::default();
    client.add_relay(relay).await.unwrap();
    client.connect().and_wait(Duration::from_secs(5)).await;
    let filter = Filter::new().kind(Kind::GiftWrap).pubkey(keys.public_key());
    let relay_url = RelayUrl::parse(relay).unwrap();
    let events = client
        .fetch_events(vec![(relay_url, vec![filter])])
        .timeout(Duration::from_secs(10))
        .await
        .unwrap();
    let mut out: Vec<UnwrappedGift> = events
        .iter()
        .filter_map(|e| nip59::extract_rumor(keys, e).ok())
        .collect();
    client.shutdown().await;
    out.sort_by_key(|g| g.rumor.created_at);
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_can_be_sent_as_a_private_message() {
    use nostr_sdk::prelude::*;
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let blossom = FakeBlossom::start().await;
    let site = FakeSite::start(PLUGINS_JSON, THEMES_JSON).await;
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "{}[share]\nservers = [\"{}\"]\nviewer = \"https://myperidot.app/s\"\n",
            gallery_config(&url, "Desk", &site),
            blossom.base
        ),
    )
    .unwrap();
    let desk = Daemon::start_configured(dir, config).await;
    desk.call("setup.start_fresh", json!(null)).await;
    until(&desk, 20, "set up", |s| s["set_up"] == json!(true)).await;
    let me = desk.status().await["identity"]["pubkey"]
        .as_str()
        .unwrap()
        .to_string();

    let friend = Keys::generate();
    let share = desk.call("share.text", json!({"text": "meet at 5"})).await;
    let link = share["url"].as_str().unwrap().to_string();
    let r = desk
        .call(
            "share.send",
            json!({"id": share["id"], "to": friend.public_key().to_bech32().unwrap()}),
        )
        .await;
    assert_eq!(r["pubkey"], json!(friend.public_key().to_hex()));

    let got = inbox(&url, &friend).await;
    assert_eq!(got.len(), 1, "one message for the friend");
    assert_eq!(got[0].sender.to_hex(), me);
    assert_eq!(got[0].rumor.kind, Kind::PrivateDirectMessage);
    assert!(
        got[0].rumor.content.contains(&link),
        "{}",
        got[0].rumor.content
    );
    assert!(got[0].rumor.content.starts_with("clipboard.txt\n"));
    // A stranger gets nothing readable.
    assert!(inbox(&url, &Keys::generate()).await.is_empty());

    // Bad addresses are refused before anything is sent.
    let mut c = crate::Client::open(&desk.socket).await;
    let e = c
        .call("share.send", json!({"id": share["id"], "to": "nobody"}))
        .await
        .unwrap_err();
    assert!(e.contains("isn't an npub"), "{e}");
    let e = c
        .call(
            "share.send",
            json!({"id": 999, "to": friend.public_key().to_hex()}),
        )
        .await
        .unwrap_err();
    assert!(e.contains("no such link"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn opal_signs_gallery_events_and_seals_messages() {
    use nostr_sdk::prelude::*;
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let blossom = FakeBlossom::start().await;
    let site = FakeSite::start(PLUGINS_JSON, THEMES_JSON).await;
    let opal = FakeOpal::start("Derek").await;
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "{}[share]\nservers = [\"{}\"]\nviewer = \"https://myperidot.app/s\"\n",
            gallery_config(&url, "Desk", &site),
            blossom.base
        ),
    )
    .unwrap();
    let desk = Daemon::start_in(dir, config, Some(&opal.socket)).await;
    desk.call("setup.use_opal", json!({})).await;
    until(&desk, 30, "set up and catalogued", |s| {
        s["set_up"] == json!(true) && s["gallery"]["plugins"] == json!(2)
    })
    .await;

    let clock = "https://github.com/derekross/omarchy-calendar";
    // A sync-only pairing can't like: the Gallery's kinds weren't declared.
    assert_eq!(desk.status().await["gallery"]["needs_enable"], json!(true));
    let mut c = crate::Client::open(&desk.socket).await;
    let e = c
        .call("gallery.like", json!({"url": clock, "on": true}))
        .await
        .unwrap_err();
    assert!(
        e.contains("Pair Peridot with Opal again") || e.contains("didn't declare"),
        "{e}"
    );
    let connects = opal.connects();
    desk.call("gallery.enable", json!(null)).await;
    assert_eq!(
        opal.connects(),
        connects + 1,
        "enabling the Gallery pairs again"
    );
    assert_eq!(desk.status().await["gallery"]["needs_enable"], json!(false));
    let signs = opal.signs();
    desk.call("gallery.like", json!({"url": clock, "on": true}))
        .await;
    assert!(opal.signs() > signs, "Opal signed the like");
    let v = desk
        .call("gallery.list", json!({"kind": "plugin", "query": "clock"}))
        .await;
    assert_eq!(v["items"][0]["liked"], json!(true));

    let friend = Keys::generate();
    let share = desk.call("share.text", json!({"text": "hi"})).await;
    let signs = opal.signs();
    desk.call(
        "share.send",
        json!({"id": share["id"], "to": friend.public_key().to_hex()}),
    )
    .await;
    assert!(opal.signs() > signs, "Opal signed the seal");
    let got = inbox(&url, &friend).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].sender, opal.keys.public_key());
}

// ── Moving a silent key into Opal ────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_key_moves_into_opal_and_sync_carries_on() {
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let opal = FakeOpal::start("Derek").await;
    let site = FakeSite::start(PLUGINS_JSON, THEMES_JSON).await;
    let desk = gallery_daemon(&url, "Desk", &site, Some(&opal.socket)).await;
    let laptop = Daemon::start(&url, "Laptop").await;
    desk.call("setup.start_fresh", json!(null)).await;
    until(&desk, 20, "desk set up", |s| s["set_up"] == json!(true)).await;
    desk.write(".config/kitty/kitty.conf", "font_size 13");
    until(&desk, 20, "published", |s| {
        s["counts"]["in_sync"] == json!(1)
    })
    .await;
    // The laptop joins through pairing, as a real second computer would.
    let offer = laptop.call("pair.new", json!(null)).await;
    desk.call("pair.join", json!({"code": offer["code"]})).await;
    until(&desk, 20, "confirm", |s| {
        s["pairing"]["stage"] == json!("confirm")
    })
    .await;
    until(&laptop, 20, "confirm", |s| {
        s["pairing"]["stage"] == json!("confirm")
    })
    .await;
    desk.call("pair.confirm", json!({"confirm": true, "matches": true}))
        .await;
    laptop
        .call("pair.confirm", json!({"confirm": true, "matches": true}))
        .await;
    until(&laptop, 20, "laptop set up", |s| s["set_up"] == json!(true)).await;
    let s = desk.status().await;
    assert_eq!(s["identity"]["mode"], json!("local"));
    assert_eq!(s["identity"]["opal_installed"], json!(true));
    let pubkey = s["identity"]["pubkey"].as_str().unwrap().to_string();

    // Too early: Opal doesn't have the key.
    let mut c = crate::Client::open(&desk.socket).await;
    let e = c
        .call(
            "identity.move.finish",
            json!({"confirm": true, "confirm": true}),
        )
        .await
        .unwrap_err();
    assert!(e.contains("doesn't have this key yet"), "{e}");

    // The code opens with the words and is this very key.
    let m = desk
        .call(
            "identity.move.start",
            json!({"confirm": true, "confirm": true}),
        )
        .await;
    let code = m["code"].as_str().unwrap().to_string();
    let words = m["words"].as_str().unwrap().to_string();
    assert!(
        code.starts_with("ncryptsec1") && words.matches('-').count() == 5,
        "{m}"
    );
    let keys = opal.add_account(&code, &words);
    assert_eq!(keys.public_key().to_hex(), pubkey);
    assert!(
        desk.call("opal.accounts", json!(null))
            .await
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["pubkey"] == json!(pubkey)),
        "the panel sees the account through opal.accounts"
    );

    // Declined pairing: nothing changes, and a retry works.
    opal.set_answer(Answer::Deny);
    let e = c
        .call(
            "identity.move.finish",
            json!({"confirm": true, "confirm": true}),
        )
        .await
        .unwrap_err();
    assert!(e.contains("declined"), "{e}");
    assert_eq!(desk.status().await["identity"]["mode"], json!("local"));
    opal.set_answer(Answer::Allow);
    let r = desk
        .call(
            "identity.move.finish",
            json!({"confirm": true, "confirm": true}),
        )
        .await;
    assert_eq!(r["label"], json!("Derek 1"));
    let s = until(&desk, 20, "opal mode", |s| {
        s["identity"]["mode"] == json!("opal")
    })
    .await;
    assert_eq!(s["identity"]["pubkey"], json!(pubkey), "same identity");
    assert_eq!(s["identity"]["name"], json!("Derek 1"));
    assert_eq!(s["counts"]["in_sync"], json!(1), "sync state kept");
    assert_eq!(opal.connects(), 1);
    // Calling it again is harmless.
    let e = c
        .call(
            "identity.move.finish",
            json!({"confirm": true, "confirm": true}),
        )
        .await
        .unwrap_err();
    assert!(e.contains("already holds"), "{e}");
    let e = c
        .call(
            "identity.move.start",
            json!({"confirm": true, "confirm": true}),
        )
        .await
        .unwrap_err();
    assert!(e.contains("already holds"), "{e}");
    let e = c
        .call("recovery.create", json!({"confirm": true, "confirm": true}))
        .await
        .unwrap_err();
    assert!(e.contains("Opal holds your key"), "{e}");

    // Settings still sync (signed by the epoch's own key, so Opal isn't
    // asked), and the laptop (same sync secret) still gets changes.
    let signs = opal.signs();
    desk.write(".config/kitty/kitty.conf", "font_size 14");
    until(&desk, 30, "published", |s| {
        s["counts"]["outgoing"] == json!(0)
    })
    .await;
    assert_eq!(
        opal.signs(),
        signs,
        "items are signed by the sync key, not Opal"
    );
    laptop.call("sync.now", json!(null)).await;
    until(&laptop, 30, "laptop sees it", |s| {
        s["counts"]["incoming"] == json!(1)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_identity_card_names_the_key_and_its_relays() {
    use nostr_sdk::prelude::{FromBech32, Nip19Profile, PublicKey};
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let site = FakeSite::start(PLUGINS_JSON, THEMES_JSON).await;
    let desk = gallery_daemon(&url, "Desk", &site, None).await;
    desk.call("setup.start_fresh", json!(null)).await;
    until(&desk, 20, "set up", |s| s["set_up"] == json!(true)).await;
    let pubkey = desk.status().await["identity"]["pubkey"]
        .as_str()
        .unwrap()
        .to_string();
    let card = desk.call("identity.card", json!(null)).await;
    assert_eq!(
        PublicKey::parse(card["npub"].as_str().unwrap())
            .unwrap()
            .to_hex(),
        pubkey
    );
    let profile = Nip19Profile::from_bech32(card["nprofile"].as_str().unwrap()).unwrap();
    assert_eq!(profile.public_key.to_hex(), pubkey);
    assert_eq!(profile.relays.len(), 1, "{card}");
    assert!(
        card["qr"]
            .as_str()
            .unwrap()
            .starts_with("data:image/svg+xml")
    );
    assert_eq!(card["has_profile"], json!(false));
    assert_eq!(card["mode"], json!("local"));
    desk.call("profile.set", json!({"name": "Derek"})).await;
    let card = desk.call("identity.card", json!(null)).await;
    assert_eq!(card["has_profile"], json!(true));
    assert_eq!(card["name"], json!("Derek"));
}

// ── Pairing v2: both sides confirm, one code, one try ────────────────

#[tokio::test(flavor = "multi_thread")]
async fn the_new_computer_can_say_no_and_a_sync_only_pairing_needs_opal() {
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let desk = Daemon::start(&url, "Desk").await;
    let laptop = Daemon::start(&url, "Laptop").await;
    desk.call("setup.start_fresh", json!(null)).await;
    until(&desk, 20, "set up", |s| s["set_up"] == json!(true)).await;

    let code = laptop.call("pair.new", json!(null)).await["code"]
        .as_str()
        .unwrap()
        .to_string();
    desk.call("pair.join", json!({"code": code})).await;
    until(&laptop, 20, "number", |s| {
        s["pairing"]["stage"] == json!("confirm")
    })
    .await;
    until(&desk, 20, "number", |s| {
        s["pairing"]["stage"] == json!("confirm")
    })
    .await;
    // The person at the new computer says No: the desk never sends.
    laptop
        .call("pair.confirm", json!({"confirm": true, "matches": false}))
        .await;
    until(&laptop, 10, "cancelled", |s| {
        s["pairing"]["stage"] == json!("cancelled")
    })
    .await;
    desk.call("pair.confirm", json!({"confirm": true, "matches": true}))
        .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(laptop.status().await["set_up"], json!(false));
    assert_eq!(
        desk.status().await["pairing"]["stage"],
        json!("waiting_other")
    );
    // The same code can't be used again.
    let mut c = crate::Client::open(&desk.socket).await;
    desk.call("pair.cancel", json!(null)).await;
    let e = c
        .call("pair.join", json!({"code": code}))
        .await
        .unwrap_err();
    assert!(e.contains("already used") || e.contains("wait"), "{e}");

    // A fresh code, but the desk keeps its key to itself: without Opal the
    // new computer can't sign, and says so.
    tokio::time::sleep(Duration::from_secs(31)).await;
    laptop.call("pair.cancel", json!(null)).await;
    let code = laptop.call("pair.new", json!(null)).await["code"]
        .as_str()
        .unwrap()
        .to_string();
    desk.call("pair.join", json!({"code": code})).await;
    until(&laptop, 20, "number", |s| {
        s["pairing"]["stage"] == json!("confirm")
    })
    .await;
    until(&desk, 20, "number", |s| {
        s["pairing"]["stage"] == json!("confirm")
    })
    .await;
    laptop
        .call("pair.confirm", json!({"confirm": true, "matches": true}))
        .await;
    desk.call(
        "pair.confirm",
        json!({"confirm": true, "matches": true, "hold_key": false}),
    )
    .await;
    let s = until(&laptop, 20, "needs Opal", |s| {
        s["pairing"]["stage"] == json!("failed")
    })
    .await;
    assert!(
        s["pairing"]["error"].as_str().unwrap().contains("Opal"),
        "{s}"
    );
    assert_eq!(laptop.status().await["set_up"], json!(false));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_computer_with_the_code_aborts_the_pairing() {
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let desk = Daemon::start(&url, "Desk").await;
    let intruder = Daemon::start(&url, "Intruder").await;
    let laptop = Daemon::start(&url, "Laptop").await;
    desk.call("setup.start_fresh", json!(null)).await;
    intruder.call("setup.start_fresh", json!(null)).await;
    until(&desk, 20, "set up", |s| s["set_up"] == json!(true)).await;
    until(&intruder, 20, "set up", |s| s["set_up"] == json!(true)).await;

    let code = laptop.call("pair.new", json!(null)).await["code"]
        .as_str()
        .unwrap()
        .to_string();
    desk.call("pair.join", json!({"code": code})).await;
    until(&laptop, 20, "number", |s| {
        s["pairing"]["stage"] == json!("confirm")
    })
    .await;
    // Someone who saw the code joins too: the new computer stops, visibly.
    intruder.call("pair.join", json!({"code": code})).await;
    let s = until(&laptop, 20, "aborted", |s| {
        s["pairing"]["stage"] == json!("aborted")
    })
    .await;
    assert!(
        s["pairing"]["error"]
            .as_str()
            .unwrap()
            .contains("another computer"),
        "{s}"
    );
    // Nothing can be shared any more, whatever the desk answers.
    desk.call("pair.confirm", json!({"confirm": true, "matches": true}))
        .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(laptop.status().await["set_up"], json!(false));
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_attempts_are_rate_limited() {
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let desk = Daemon::start(&url, "Desk").await;
    desk.call("setup.start_fresh", json!(null)).await;
    until(&desk, 20, "set up", |s| s["set_up"] == json!(true)).await;
    let mut c = crate::Client::open(&desk.socket).await;
    for _ in 0..5 {
        let code = peridot_sync::pairing::Code::generate().display();
        c.call("pair.join", json!({"code": code})).await.unwrap();
        c.call("pair.cancel", json!(null)).await.unwrap();
    }
    let code = peridot_sync::pairing::Code::generate().display();
    let e = c
        .call("pair.join", json!({"code": code}))
        .await
        .unwrap_err();
    assert!(e.contains("too many"), "{e}");
}

// ── Epochs: removal, adoption, migration, recovery ───────────────────

/// Three computers on one identity, paired in a chain.
async fn three_paired(url: &str) -> (Daemon, Daemon, Daemon) {
    let desk = Daemon::start(url, "Desk").await;
    let laptop = Daemon::start(url, "Laptop").await;
    let spare = Daemon::start(url, "Spare").await;
    desk.write(".config/kitty/kitty.conf", "font_size 13");
    desk.call("setup.start_fresh", json!(null)).await;
    until(&desk, 20, "desk published", |s| {
        s["counts"]["in_sync"] == json!(1)
    })
    .await;
    for joiner in [&laptop, &spare] {
        let code = joiner.call("pair.new", json!(null)).await["code"]
            .as_str()
            .unwrap()
            .to_string();
        desk.call("pair.join", json!({"code": code})).await;
        until(&desk, 20, "number", |s| {
            s["pairing"]["stage"] == json!("confirm")
        })
        .await;
        until(joiner, 20, "number", |s| {
            s["pairing"]["stage"] == json!("confirm")
        })
        .await;
        desk.call("pair.confirm", json!({"confirm": true, "matches": true}))
            .await;
        joiner
            .call("pair.confirm", json!({"confirm": true, "matches": true}))
            .await;
        until(joiner, 20, "joined", |s| s["set_up"] == json!(true)).await;
        desk.call("pair.cancel", json!(null)).await;
    }
    // Everyone has announced (the desk knows both device keys).
    until(&desk, 30, "desk sees three computers", |s| {
        s["devices"].as_array().map(Vec::len) == Some(3)
    })
    .await;
    (desk, laptop, spare)
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_a_computer_rotates_and_it_stops_receiving() {
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let (desk, laptop, spare) = three_paired(&url).await;
    until(&spare, 30, "spare has the file", |s| {
        s["counts"]["incoming"] == json!(1)
    })
    .await;
    until(&laptop, 30, "laptop has the file", |s| {
        s["counts"]["incoming"] == json!(1)
    })
    .await;
    spare
        .call("apply", json!({"paths": [".config/kitty/kitty.conf"]}))
        .await;
    for d in [&desk, &laptop, &spare] {
        assert_eq!(d.status().await["identity"]["epoch"], json!(1));
    }

    // The desk removes the laptop: a new epoch for the desk and the spare.
    let laptop_id = laptop.status().await["device_id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = desk.call("device.remove", json!({"id": laptop_id})).await;
    assert_eq!(r["epoch"], json!(2));
    until(&desk, 30, "desk on epoch 2", |s| {
        s["identity"]["epoch"] == json!(2)
    })
    .await;
    until(&spare, 60, "spare adopts epoch 2", |s| {
        s["identity"]["epoch"] == json!(2)
    })
    .await;
    let s = until(&laptop, 60, "laptop told it was removed", |s| {
        s["error"].as_str().is_some_and(|e| e.contains("removed"))
    })
    .await;
    assert_eq!(s["set_up"], json!(false), "{s}");
    assert!(desk.status().await["identity"]["window_until"].is_u64());

    // New changes reach the spare and never the laptop.
    desk.write(".config/kitty/kitty.conf", "font_size 14");
    until(&spare, 60, "spare gets the change", |s| {
        s["files"]
            .as_array()
            .is_some_and(|f| f.iter().any(|f| f["status"] == json!("incoming")))
    })
    .await;
    spare
        .call("apply", json!({"paths": [".config/kitty/kitty.conf"]}))
        .await;
    assert_eq!(
        spare.read(".config/kitty/kitty.conf").unwrap(),
        "font_size 14"
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        laptop.read(".config/kitty/kitty.conf").is_none(),
        "the laptop never applied anything"
    );
    // And the spare's own changes go under the new epoch too.
    spare.write(".config/kitty/kitty.conf", "font_size 15");
    until(&desk, 60, "desk gets the spare's change", |s| {
        s["counts"]["incoming"] == json!(1)
    })
    .await;
    assert_eq!(
        desk.status().await["devices"].as_array().map(Vec::len),
        Some(2),
        "the laptop is gone from the list"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_opal_identity_rotates_without_prompts() {
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let opal = FakeOpal::start("Derek").await;
    let desk = Daemon::start_with(&url, "Desk", Some(&opal.socket)).await;
    desk.call("setup.use_opal", json!({})).await;
    until(&desk, 20, "set up", |s| {
        s["set_up"] == json!(true) && s["error"].is_null()
    })
    .await;
    desk.write(".config/kitty/kitty.conf", "font_size 13");
    until(&desk, 20, "published", |s| {
        s["counts"]["in_sync"] == json!(1)
    })
    .await;
    // Opal only ever signs the root event (kind 30078) and encrypts to
    // itself; deletions and items are the epoch key's business.
    opal.set_answer(Answer::DenySensitive);
    let signs = opal.signs();
    let r = desk.call("sync.rotate", json!(null)).await;
    assert_eq!(r["epoch"], json!(2));
    let s = until(&desk, 30, "epoch 2, everything republished", |s| {
        s["identity"]["epoch"] == json!(2)
            && s["counts"]["in_sync"] == json!(1)
            && s["error"].is_null()
    })
    .await;
    assert!(s["identity"]["window_until"].is_u64());
    // The new epoch's root event is the one thing Opal signs.
    for _ in 0..50 {
        if opal.signs() > signs {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(opal.signs(), signs + 1, "one signature: the root event");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recovery_kit_restores_after_a_rotation() {
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let old = Daemon::start(&url, "Old").await;
    old.write(".config/kitty/kitty.conf", "font_size 13");
    old.call("setup.start_fresh", json!(null)).await;
    until(&old, 20, "publish", |s| s["counts"]["in_sync"] == json!(1)).await;
    let kit = old.call("recovery.create", json!({"confirm": true})).await;
    old.call("sync.rotate", json!(null)).await;
    until(&old, 30, "epoch 2", |s| {
        s["identity"]["epoch"] == json!(2) && s["counts"]["in_sync"] == json!(1)
    })
    .await;
    old.write(".config/kitty/kitty.conf", "font_size 14");
    until(&old, 20, "published under epoch 2", |s| {
        s["counts"]["outgoing"] == json!(0)
    })
    .await;

    let new = Daemon::start(&url, "New").await;
    new.call(
        "recovery.restore",
        json!({"code": kit["code"], "words": kit["words"]}),
    )
    .await;
    let s = until(&new, 30, "restored on the current epoch", |s| {
        s["set_up"] == json!(true) && s["counts"]["incoming"] == json!(1)
    })
    .await;
    assert_eq!(s["identity"]["epoch"], json!(2), "{s}");
    new.call("apply", json!({"paths": [".config/kitty/kitty.conf"]}))
        .await;
    assert_eq!(
        new.read(".config/kitty/kitty.conf").unwrap(),
        "font_size 14"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn first_version_data_is_moved_to_epoch_one() {
    use nostr_sdk::prelude::*;
    use peridot_sync::identity::Identity;
    let relay = mock_relay().await;
    let url = relay.url().await.to_string();
    let relay_url = RelayUrl::parse(&url).unwrap();

    // Settings published by the first protocol version: items and root
    // signed by the identity, sealed with a legacy (epoch 0) secret.
    let keys = Keys::generate();
    let legacy = Identity::from_keys(keys.clone()).with_secret(
        peridot_sync::crypto::SyncSecret::legacy(peridot_sync::crypto::random_bytes()),
    );
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".config/kitty")).unwrap();
    std::fs::write(home.path().join(".config/kitty/kitty.conf"), "font_size 13").unwrap();
    let db = opal_core::db::Db::open_in_memory().unwrap();
    let engine = peridot_sync::sync::SyncEngine::new(peridot_sync::sync::SyncParams {
        identity: legacy.clone(),
        previous: None,
        signer: std::sync::Arc::new(peridot_sync::signer::LocalSigner(keys.clone())),
        store: peridot_sync::store::SyncStore::new(db.clone()).unwrap(),
        outbox: opal_kit::relays::Outbox::new(db).unwrap(),
        home: peridot_sync::apply::Home::open(home.path()).unwrap(),
        manifest: peridot_sync::manifest::Manifest::new(Default::default()),
        client: nostr_sdk::client::Client::default(),
        relays: vec![relay_url.clone()],
        backups_dir: home.path().join("backups"),
        device_name: "Old desk".into(),
        version: "0.1.1".into(),
    })
    .unwrap();
    engine.connect().await;
    engine.catch_up().await.unwrap();
    assert_eq!(engine.epoch(), 0);
    engine.publish_root().await.unwrap();
    engine.announce().await.unwrap();
    let report = engine.publish_changes().await.unwrap();
    assert_eq!(report.published.len(), 1);
    assert_eq!(
        engine.sync_pubkey(),
        keys.public_key(),
        "legacy items are the identity's"
    );
    engine.client().shutdown().await;

    // A new-version computer takes the key: it reads the legacy root, then
    // moves everything to epoch 1 under the sync key.
    let desk = Daemon::start(&url, "Desk").await;
    desk.call(
        "setup.import",
        json!({"secret": keys.secret_key().to_secret_hex()}),
    )
    .await;
    let s = until(&desk, 40, "moved to epoch 1", |s| {
        s["set_up"] == json!(true) && s["identity"]["epoch"] == json!(1) && s["error"].is_null()
    })
    .await;
    assert_eq!(
        s["counts"]["incoming"],
        json!(1),
        "the old setting came along: {s}"
    );
    assert!(s["identity"]["window_until"].is_u64());
    // The root on the servers is the new form now.
    let client = nostr_sdk::client::Client::default();
    client.add_relay(&relay_url).await.unwrap();
    client.connect().and_wait(Duration::from_secs(3)).await;
    let root_filter = Filter::new()
        .author(keys.public_key())
        .kind(Kind::Custom(30078))
        .identifier(Identity::root_name(&keys.public_key()));
    let mut root = None;
    for _ in 0..100 {
        let roots = client
            .fetch_events(vec![(relay_url.clone(), vec![root_filter.clone()])])
            .timeout(Duration::from_secs(5))
            .await
            .unwrap();
        root = roots.iter().max_by_key(|e| e.created_at).cloned();
        if root
            .as_ref()
            .is_some_and(|r| Identity::root_commitment(r).is_some())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        root.as_ref()
            .is_some_and(|r| Identity::root_commitment(r).is_some()),
        "root is v2: {root:?}"
    );
    client.shutdown().await;

    // A second old-version computer that comes back is told to pair again.
    let stale = Daemon::start(&url, "Stale").await;
    stale
        .call(
            "setup.import",
            json!({"secret": keys.secret_key().to_secret_hex()}),
        )
        .await;
    let s = until(&stale, 40, "stale told to pair again", |s| {
        s["identity"]["epoch"] == json!(1)
            || s["error"].as_str().is_some_and(|e| e.contains("Pair"))
    })
    .await;
    // Importing the key reads the current (v2) root, so it simply joins
    // epoch 1; only a computer still holding a legacy secret is stale.
    assert_eq!(s["identity"]["epoch"], json!(1), "{s}");
}
