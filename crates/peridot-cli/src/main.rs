//! `peridot`: keep your Omarchy computers matching, from a terminal.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use opal_core::ipc::{IpcMessage, IpcRequest};
use opal_core::paths::AppDirs;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Parser)]
#[command(
    name = "peridot",
    version,
    about = "Keep your Omarchy computers matching"
)]
struct Cli {
    /// Control socket path.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// Print raw JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// What's in sync, waiting or in conflict (the default).
    Status,
    /// Set up Peridot on this computer. With Opal installed, offers its
    /// identity; otherwise makes a new one.
    Start {
        /// Use the identity Opal holds (optionally which account, by name
        /// or npub/pubkey prefix).
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        opal: Option<String>,
        /// Use a key you already have (asks for the nsec/ncryptsec).
        #[arg(long)]
        import: bool,
        /// Make a brand-new identity even if Opal is installed.
        #[arg(long)]
        fresh: bool,
    },
    /// Pair a new computer. Run on the new one; then run
    /// `peridot pair <code>` on a computer you already use.
    Pair {
        /// The code shown on the new computer.
        code: Option<String>,
    },
    /// Apply incoming settings (all, or just these paths).
    Apply {
        paths: Vec<String>,
    },
    /// Resolve a conflict by keeping this computer's version.
    KeepMine {
        path: String,
    },
    /// Undo the last apply (or a specific one from `peridot history`).
    Undo {
        id: Option<i64>,
    },
    /// Recent applies.
    History,
    /// Check for changes now.
    Sync,
    /// Stop publishing changes from this computer.
    Pause,
    Resume,
    /// Your computers.
    Devices,
    /// The servers your encrypted settings are stored on.
    Relays {
        #[command(subcommand)]
        cmd: Option<RelaysCmd>,
    },
    /// Remove a computer from your list.
    RemoveDevice {
        id: String,
    },
    /// Make a recovery kit (six words and a recovery code).
    Recovery,
    /// Restore from a recovery kit on a new computer.
    Restore,
    /// Stop syncing on this computer (your others keep going).
    Leave,
}

#[derive(Subcommand)]
enum RelaysCmd {
    /// Add a relay (wss://…); it's checked first.
    Add {
        url: String,
    },
    Remove {
        url: String,
    },
}

struct Conn {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    next_id: u64,
}

impl Conn {
    async fn open(path: &PathBuf) -> Result<Self> {
        let stream = UnixStream::connect(path).await.with_context(|| {
            format!(
                "can't reach Peridot at {} (start it with `systemctl --user start peridot`)",
                path.display()
            )
        })?;
        let (r, w) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(r),
            writer: w,
            next_id: 1,
        })
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = serde_json::to_string(&IpcRequest {
            id,
            method: method.into(),
            params,
        })?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        loop {
            if let IpcMessage::Response(r) = self.read().await?
                && r.id == id
            {
                return match r.error {
                    Some(e) => Err(anyhow!(e)),
                    None => Ok(r.result.unwrap_or(Value::Null)),
                };
            }
        }
    }

    async fn read(&mut self) -> Result<IpcMessage> {
        let mut line = String::new();
        if self.reader.read_line(&mut line).await? == 0 {
            bail!("peridotd closed the connection");
        }
        Ok(serde_json::from_str(&line)?)
    }

    /// The next pairing state pushed by the daemon.
    async fn next_pairing(&mut self) -> Result<Value> {
        loop {
            if let IpcMessage::Event(ev) = self.read().await?
                && ev.event == "state"
                && !ev.data["pairing"].is_null()
            {
                return Ok(ev.data["pairing"].clone());
            }
        }
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("peridot: {e:#}");
        std::process::exit(1);
    }
}

fn ask(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn yes(prompt: &str) -> Result<bool> {
    Ok(ask(&format!("{prompt} [y/N] "))?
        .to_lowercase()
        .starts_with('y'))
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let socket = cli
        .socket
        .clone()
        .unwrap_or_else(|| AppDirs::PERIDOT.socket_path());
    let mut c = Conn::open(&socket).await?;
    let out = |v: &Value| println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());

    match cli.cmd.unwrap_or(Cmd::Status) {
        Cmd::Status => {
            let s = c.call("status", json!(null)).await?;
            if cli.json {
                out(&s);
            } else {
                print_status(&s);
            }
        }
        Cmd::Start {
            opal,
            import,
            fresh,
        } => {
            let accounts = c.call("opal.accounts", json!(null)).await?;
            let accounts = accounts.as_array().cloned().unwrap_or_default();
            let opal_pick = |wanted: &str| -> Option<Value> {
                if wanted.is_empty() {
                    return accounts
                        .iter()
                        .find(|a| a["current"] == json!(true))
                        .or(accounts.first())
                        .cloned();
                }
                let w = wanted.to_lowercase();
                accounts
                    .iter()
                    .find(|a| {
                        a["label"].as_str().is_some_and(|l| l.to_lowercase() == w)
                            || a["pubkey"].as_str().is_some_and(|p| p.starts_with(&w))
                            || a["npub"].as_str().is_some_and(|n| n.starts_with(&w))
                    })
                    .cloned()
            };
            if import {
                let secret = rpassword::prompt_password(
                    "Your key (nsec, hex, ncryptsec or recovery phrase): ",
                )?;
                let mut params = json!({"secret": secret.trim()});
                if secret.trim().starts_with("ncryptsec1") {
                    params["password"] =
                        json!(rpassword::prompt_password("Password of that ncryptsec: ")?);
                }
                c.call("setup.import", params).await?;
                println!("Peridot is set up with your key.");
            } else if let Some(wanted) = opal {
                let account = opal_pick(&wanted).ok_or_else(|| {
                    anyhow!(if accounts.is_empty() {
                        "Opal isn't running or has no key yet".to_string()
                    } else {
                        format!("Opal has no account matching \"{wanted}\"")
                    })
                })?;
                c.call("setup.use_opal", json!({"pubkey": account["pubkey"]}))
                    .await?;
                println!(
                    "Peridot is set up with your Opal identity ({}).",
                    account["label"].as_str().unwrap_or("")
                );
            } else if !fresh && let Some(account) = opal_pick("") {
                let label = account["label"].as_str().unwrap_or("").to_string();
                if yes(&format!(
                    "Use your Opal identity \"{label}\"? (No makes a new one)"
                ))? {
                    c.call("setup.use_opal", json!({"pubkey": account["pubkey"]}))
                        .await?;
                    println!("Peridot is set up with your Opal identity ({label}).");
                } else {
                    c.call("setup.start_fresh", json!(null)).await?;
                    println!("Peridot is set up with a new identity.");
                }
            } else {
                c.call("setup.start_fresh", json!(null)).await?;
                println!("Peridot is set up with a new identity.");
            }
            println!(
                "Your settings now sync from this computer. Add another with `peridot pair` on it."
            );
            println!("Tip: `peridot recovery` makes a recovery kit.");
        }
        Cmd::Relays { cmd } => {
            let s = c.call("status", json!(null)).await?;
            let mut relays: Vec<String> = s["relays"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|r| r.as_str().map(String::from))
                .collect();
            match cmd {
                None => {
                    for r in &relays {
                        println!("{r}");
                    }
                }
                Some(RelaysCmd::Add { url }) => {
                    let url = url.trim().trim_end_matches('/').to_string();
                    println!("Checking {url}…");
                    let check = c.call("relays.check", json!({"url": url})).await?;
                    if check["reachable"] != json!(true) {
                        bail!("couldn't connect to {url}");
                    }
                    if check["auth_required"] == json!(true) {
                        println!("  (it asks for a login; Peridot handles that)");
                    }
                    if !relays.contains(&url) {
                        relays.push(url.clone());
                    }
                    c.call("relays.set", json!({"relays": relays})).await?;
                    println!("Added {url}. Your settings will be stored there too.");
                }
                Some(RelaysCmd::Remove { url }) => {
                    let url = url.trim().trim_end_matches('/');
                    let before = relays.len();
                    relays.retain(|r| r != url);
                    if relays.len() == before {
                        bail!("{url} isn't in the list");
                    }
                    c.call("relays.set", json!({"relays": relays})).await?;
                    println!("Removed {url}.");
                }
            }
        }
        Cmd::Pair { code: None } => {
            c.call("subscribe", json!(null)).await?;
            let v = c.call("pair.new", json!(null)).await?;
            println!("On a computer that already uses Peridot, run:\n");
            println!("    peridot pair {}\n", v["code"].as_str().unwrap_or(""));
            println!(
                "(or choose \"Pair a new computer\" in its panel). The code works for 5 minutes."
            );
            loop {
                let p = c.next_pairing().await?;
                match p["stage"].as_str().unwrap_or("") {
                    "confirm" => println!(
                        "\nCheck that {} shows this number: {}",
                        p["other"].as_str().unwrap_or("the other computer"),
                        p["number"].as_str().unwrap_or("")
                    ),
                    "done" => {
                        println!(
                            "\nPaired. Your settings are arriving; review them with `peridot status`."
                        );
                        break;
                    }
                    "expired" => bail!("the code expired; run `peridot pair` again"),
                    "failed" => bail!("{}", p["error"].as_str().unwrap_or("pairing failed")),
                    _ => {}
                }
            }
        }
        Cmd::Pair { code: Some(code) } => {
            c.call("subscribe", json!(null)).await?;
            c.call("pair.join", json!({"code": code})).await?;
            println!("Waiting for the new computer…");
            loop {
                let p = c.next_pairing().await?;
                match p["stage"].as_str().unwrap_or("") {
                    "confirm" => {
                        let ok = yes(&format!(
                            "Does {} show the number {}?",
                            p["other"].as_str().unwrap_or("the new computer"),
                            p["number"].as_str().unwrap_or("")
                        ))?;
                        c.call("pair.confirm", json!({"matches": ok})).await?;
                        if !ok {
                            bail!("pairing cancelled; nothing was shared");
                        }
                    }
                    "done" => {
                        println!("Paired.");
                        break;
                    }
                    "expired" => {
                        bail!("the new computer didn't answer; check the code and try again")
                    }
                    "failed" => bail!("{}", p["error"].as_str().unwrap_or("pairing failed")),
                    "cancelled" => bail!("pairing cancelled"),
                    _ => {}
                }
            }
        }
        Cmd::Apply { paths } => {
            let r = c.call("apply", json!({"paths": paths})).await?;
            let applied = r["applied"].as_array().map(|a| a.len()).unwrap_or(0);
            println!("Applied {applied}.");
            if let Some(failed) = r["failed"].as_array() {
                for f in failed {
                    println!(
                        "  couldn't apply {}: {}",
                        f[0].as_str().unwrap_or(""),
                        f[1].as_str().unwrap_or("")
                    );
                }
            }
            if applied > 0 {
                println!("Changed your mind? `peridot undo`");
            }
        }
        Cmd::KeepMine { path } => {
            c.call("conflict.keep_local", json!({"path": path})).await?;
            println!("Kept this computer's version; your other computers will be offered it.");
        }
        Cmd::Undo { id } => {
            let id = match id {
                Some(id) => id,
                None => {
                    let s = c.call("status", json!(null)).await?;
                    s["history"]
                        .as_array()
                        .and_then(|h| h.iter().find(|e| e["undone"] == json!(false)))
                        .and_then(|e| e["id"].as_i64())
                        .ok_or_else(|| anyhow!("nothing to undo"))?
                }
            };
            let r = c.call("history.undo", json!({"id": id})).await?;
            println!(
                "Put back {} file(s) on this computer. Your other computers keep theirs.",
                r["restored"].as_array().map(|a| a.len()).unwrap_or(0)
            );
        }
        Cmd::History => {
            let s = c.call("status", json!(null)).await?;
            for h in s["history"].as_array().into_iter().flatten() {
                println!(
                    "{:>4}  {}{}",
                    h["id"],
                    h["summary"].as_str().unwrap_or(""),
                    if h["undone"] == json!(true) {
                        " (undone)"
                    } else {
                        ""
                    }
                );
            }
        }
        Cmd::Sync => {
            c.call("sync.now", json!(null)).await?;
            println!("Checking for changes.");
        }
        Cmd::Pause => {
            c.call("sync.pause", json!({"paused": true})).await?;
            println!("Paused: changes here aren't published until `peridot resume`.");
        }
        Cmd::Resume => {
            c.call("sync.pause", json!({"paused": false})).await?;
            println!("Syncing again.");
        }
        Cmd::Devices => {
            let s = c.call("status", json!(null)).await?;
            for d in s["devices"].as_array().into_iter().flatten() {
                println!(
                    "{}  {}{}",
                    d["id"].as_str().unwrap_or(""),
                    d["name"].as_str().unwrap_or(""),
                    if d["this"] == json!(true) {
                        "  (this computer)"
                    } else {
                        ""
                    }
                );
            }
        }
        Cmd::RemoveDevice { id } => {
            c.call("device.remove", json!({"id": id})).await?;
            println!("Removed.");
        }
        Cmd::Recovery => {
            println!("Making your recovery kit…");
            let r = c.call("recovery.create", json!(null)).await?;
            println!("\nWrite down these six words and keep them somewhere safe:\n");
            println!("    {}\n", r["words"].as_str().unwrap_or(""));
            let saved = c
                .call("recovery.save_page", json!({"code": r["code"]}))
                .await?;
            println!(
                "The recovery code is saved to {}",
                saved["path"].as_str().unwrap_or("")
            );
            println!("(print it, or keep a copy somewhere other than this computer).");
            println!("The words aren't in that file: you need both to restore.");
        }
        Cmd::Restore => {
            let code = ask("Recovery code (ncryptsec1…): ")?;
            let words = ask("The six words: ")?;
            println!("Restoring…");
            c.call("recovery.restore", json!({"code": code, "words": words}))
                .await?;
            println!("Restored. Your settings are arriving; review them with `peridot status`.");
        }
        Cmd::Leave => {
            if yes("Stop syncing on this computer? Your settings here stay as they are.")? {
                c.call("setup.leave", json!(null)).await?;
                println!("Done. Your other computers keep syncing.");
            }
        }
    }
    Ok(())
}

fn print_status(s: &Value) {
    if s["set_up"] != json!(true) {
        println!("Peridot isn't set up on this computer.");
        println!("  First computer:            peridot start");
        println!("  You already use Peridot:   peridot pair");
        println!("  Restore from a kit:        peridot restore");
        return;
    }
    let n = |k: &str| s["counts"][k].as_u64().unwrap_or(0);
    let id = &s["identity"];
    let who = match id["name"].as_str().filter(|x| !x.is_empty()) {
        Some(name) => name.to_string(),
        None => id["npub"]
            .as_str()
            .map(|n| format!("{}…{}", &n[..12], &n[n.len() - 6..]))
            .unwrap_or_default(),
    };
    println!(
        "{} · {}{} · {} in sync",
        s["device_name"].as_str().unwrap_or(""),
        who,
        if id["mode"] == json!("opal") {
            " (via Opal)"
        } else {
            ""
        },
        n("in_sync")
    );
    if s["paused"] == json!(true) {
        println!("Paused.");
    }
    for f in s["files"].as_array().into_iter().flatten() {
        let status = f["status"].as_str().unwrap_or("");
        let from = f["from"].as_str().unwrap_or("another computer");
        let line = match status {
            "incoming" if f["deleted"] == json!(true) => format!("deleted on {from}"),
            "incoming" => format!("changed on {from}"),
            "conflict" => format!("changed here and on {from}"),
            "outgoing" => "changed here, sending".into(),
            "kept" => format!("kept this computer's version (undid {from}'s)"),
            _ => continue,
        };
        let warn = if f["runs_commands"] == json!(true) {
            "  (can run commands)"
        } else {
            ""
        };
        println!("  {:<48} {line}{warn}", f["path"].as_str().unwrap_or(""));
    }
    for o in s["offers"].as_array().into_iter().flatten() {
        let from = o["from"].as_str().unwrap_or("another computer");
        match o["kind"].as_str().unwrap_or("") {
            "theme" => println!(
                "  Theme {} is in use on {from}",
                o["name"].as_str().unwrap_or("")
            ),
            "install_theme" => println!(
                "  Theme {} is installed on {from}",
                o["name"].as_str().unwrap_or("")
            ),
            "install_plugin" => println!(
                "  Plugin {} is installed on {from}",
                o["name"].as_str().unwrap_or("")
            ),
            _ => {}
        }
    }
    if n("incoming") > 0 {
        println!("Apply with `peridot apply` (undo with `peridot undo`).");
    }
    if n("conflicts") > 0 {
        println!("Conflicts: `peridot keep-mine <path>`, or `peridot apply <path>` to use theirs.");
    }
    if n("skipped") > 0 {
        println!(
            "{} file(s) skipped (links or possible secrets); see `peridot --json status`.",
            n("skipped")
        );
    }
    if let Some(e) = s["error"].as_str() {
        println!("Problem: {e}");
    }
}
