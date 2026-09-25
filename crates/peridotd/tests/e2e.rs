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
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            format!("relays = [\"{relay}\"]\ndevice_name = \"{name}\"\n"),
        )
        .unwrap();
        let socket = dir.path().join("peridot.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_peridotd"))
            .args(["--memory-keyring", "--socket"])
            .arg(&socket)
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
    let relay = MockRelay::run().await.unwrap();
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
    let relay = MockRelay::run().await.unwrap();
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
    let relay = MockRelay::run().await.unwrap();
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
    let relay = MockRelay::run().await.unwrap();
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
