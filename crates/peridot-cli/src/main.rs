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
    /// Your pairing with Opal (when Opal holds your identity).
    Opal {
        #[command(subcommand)]
        cmd: OpalCmd,
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
    /// Share a file as a private link (encrypted; the link holds the key).
    Share {
        /// Files to share. Without any: the clipboard, the last screenshot,
        /// or a file chooser (--clipboard / --screenshot / --pick).
        paths: Vec<PathBuf>,
        /// Share what's on the clipboard (text or an image).
        #[arg(long)]
        clipboard: bool,
        /// Share the most recent screenshot.
        #[arg(long)]
        screenshot: bool,
        /// Choose files with the desktop file chooser.
        #[arg(long)]
        pick: bool,
        /// How many days the link works (default from settings, 7).
        #[arg(long)]
        days: Option<u32>,
        /// Show a desktop notification instead of printing (for menus).
        #[arg(long)]
        notify: bool,
        /// Also send the link to someone as a private message (an npub or
        /// a name@domain address).
        #[arg(long)]
        to: Option<String>,
    },
    /// Your private links.
    Shares,
    /// Send one of your links to someone as a private message.
    Send {
        /// The link's number from `peridot shares`.
        id: i64,
        /// An npub or a name@domain address.
        to: String,
    },
    /// The Gallery: themes, plugins and setups from other Omarchy users.
    Gallery {
        /// Words to look for.
        query: Vec<String>,
        /// Only themes.
        #[arg(long)]
        themes: bool,
        /// Only plugins.
        #[arg(long)]
        plugins: bool,
        /// Setups people published.
        #[arg(long)]
        setups: bool,
        /// Sort by "top" (liked by people you trust), "stars" or "name".
        #[arg(long, default_value = "top")]
        sort: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Like a theme or plugin (by repository address).
    Like {
        url: String,
        /// Take the like back.
        #[arg(long)]
        undo: bool,
    },
    /// Review a theme or plugin.
    Review {
        url: String,
        text: String,
        /// 1 to 5.
        #[arg(long)]
        rating: Option<u8>,
    },
    /// Install a theme or plugin from the Gallery.
    Install {
        url: String,
    },
    /// Follow someone whose reviews or setups you like.
    Follow {
        /// An npub or a name@domain address.
        who: String,
        #[arg(long)]
        undo: bool,
    },
    /// Publish this computer's theme and plugins as a setup others can
    /// install in one go.
    PublishSetup {
        title: String,
        #[arg(long, default_value = "")]
        summary: String,
        /// Attach this picture (uploaded for everyone to see).
        #[arg(long)]
        screenshot: Option<PathBuf>,
    },
    /// Install someone's setup (its address from `peridot gallery --setups`).
    InstallSetup {
        coordinate: String,
    },
    /// The name others see in the Gallery (only if you don't have one yet).
    Name {
        name: String,
    },
    /// Remove a private link and its file from the server.
    Unshare {
        id: i64,
    },
    /// Your computers.
    Devices,
    /// The servers your encrypted settings are stored on.
    Relays {
        #[command(subcommand)]
        cmd: Option<RelaysCmd>,
    },
    /// Check your sync servers now: send again what any of them lacks,
    /// refresh what's getting old, and drop old chunks nothing refers to.
    Tidy,
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
enum OpalCmd {
    /// Pair (again) with Opal: approve Peridot in Opal's bar.
    Pair,
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
                println!("{OPAL_WILL_ASK}");
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
                    println!("{OPAL_WILL_ASK}");
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
        Cmd::Share {
            paths,
            clipboard,
            screenshot,
            pick,
            days,
            notify,
            to,
        } => {
            let mut targets: Vec<(String, Option<PathBuf>)> = Vec::new(); // (name, path) or text
            let mut text: Option<String> = None;
            if clipboard {
                let types = String::from_utf8_lossy(
                    &std::process::Command::new("wl-paste")
                        .arg("--list-types")
                        .output()?
                        .stdout,
                )
                .to_string();
                if let Some(t) = types.lines().find(|t| t.starts_with("image/")) {
                    let out = std::process::Command::new("wl-paste")
                        .args(["--type", t])
                        .output()?;
                    anyhow::ensure!(!out.stdout.is_empty(), "the clipboard is empty");
                    let ext = t
                        .split('/')
                        .nth(1)
                        .unwrap_or("png")
                        .split('+')
                        .next()
                        .unwrap_or("png");
                    let dir = share_tmp_dir()?;
                    let path = dir.join(format!("clipboard-{}.{ext}", now_secs()));
                    std::fs::write(&path, &out.stdout)?;
                    targets.push((
                        path.file_name().unwrap().to_string_lossy().into_owned(),
                        Some(path),
                    ));
                } else {
                    let out = std::process::Command::new("wl-paste")
                        .arg("--no-newline")
                        .output()?;
                    let t = String::from_utf8_lossy(&out.stdout).to_string();
                    anyhow::ensure!(!t.trim().is_empty(), "the clipboard is empty");
                    text = Some(t);
                }
            } else if screenshot {
                let path = last_screenshot().ok_or_else(|| anyhow!("no screenshot found"))?;
                targets.push((
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    Some(path),
                ));
            } else if pick || paths.is_empty() {
                let out = std::process::Command::new("omarchy-file-select")
                    .args(["--title", "Share as a private link", "--multiple"])
                    .output()?;
                if !out.status.success() && out.status.code().unwrap_or(1) > 1 {
                    bail!("the file chooser did not open");
                }
                for line in String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                {
                    let p = PathBuf::from(line.trim());
                    targets.push((
                        p.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        Some(p),
                    ));
                }
                if targets.is_empty() {
                    return Ok(());
                }
            } else {
                for p in paths {
                    let p =
                        std::fs::canonicalize(&p).with_context(|| format!("{}", p.display()))?;
                    targets.push((
                        p.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        Some(p),
                    ));
                }
            }

            let mut links: Vec<(String, String)> = Vec::new();
            let mut ids: Vec<i64> = Vec::new();
            // With Opal holding the key, an upload may wait on its prompt.
            let via_opal = c
                .call("status", json!(null))
                .await
                .is_ok_and(|s| s["identity"]["mode"] == json!("opal"));
            let hint = tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                if !via_opal {
                    return;
                }
                if notify {
                    let _ = std::process::Command::new("omarchy-notification-send")
                        .args([
                            "--app-name",
                            "Peridot",
                            "-g",
                            "󰓦",
                            "-t",
                            "6000",
                            "Approve Peridot in Opal",
                            "Opal (in your bar) is asking whether Peridot may sign the upload.",
                        ])
                        .status();
                } else {
                    println!("Approve Peridot in Opal (in your bar) to finish the upload…");
                }
            });
            let result: Result<()> = async {
                if let Some(t) = text {
                    let s = c
                        .call("share.text", json!({"text": t, "expire_days": days}))
                        .await?;
                    links.push((
                        s["name"].as_str().unwrap_or("clipboard").to_string(),
                        s["url"].as_str().unwrap_or("").to_string(),
                    ));
                    ids.push(s["id"].as_i64().unwrap_or(0));
                }
                for (name, path) in targets {
                    let s = c
                        .call(
                            "share.file",
                            json!({"path": path.unwrap(), "name": name, "expire_days": days}),
                        )
                        .await?;
                    links.push((
                        s["name"].as_str().unwrap_or("").to_string(),
                        s["url"].as_str().unwrap_or("").to_string(),
                    ));
                    ids.push(s["id"].as_i64().unwrap_or(0));
                }
                if let Some(who) = &to {
                    for id in &ids {
                        let r = c.call("share.send", json!({"id": id, "to": who})).await?;
                        if !notify {
                            println!("Sent to {}.", r["to"].as_str().unwrap_or(who));
                        }
                    }
                }
                Ok(())
            }
            .await;
            hint.abort();
            if let Err(e) = result {
                if notify {
                    let _ = std::process::Command::new("omarchy-notification-send")
                        .args([
                            "--app-name",
                            "Peridot",
                            "-g",
                            "󰓦",
                            "-u",
                            "critical",
                            "Couldn't share",
                            &e.to_string(),
                        ])
                        .status();
                }
                return Err(e);
            }
            let all: Vec<String> = links.iter().map(|(_, u)| u.clone()).collect();
            let _ = std::process::Command::new("wl-copy")
                .arg("--")
                .arg(all.join("\n"))
                .status();
            if notify {
                let body = if links.len() == 1 {
                    format!("{} · link copied to the clipboard", links[0].0)
                } else {
                    format!("{} links copied to the clipboard", links.len())
                };
                let _ = std::process::Command::new("omarchy-notification-send")
                    .args([
                        "--app-name",
                        "Peridot",
                        "-g",
                        "󰓦",
                        "-t",
                        "8000",
                        "Private link ready",
                        &body,
                    ])
                    .status();
            } else {
                for (name, url) in &links {
                    println!("{name}\n  {url}");
                }
                println!(
                    "Copied to the clipboard. Anyone with the link can open it until it expires; `peridot unshare` removes it sooner."
                );
            }
        }
        Cmd::Tidy => {
            println!("Checking your sync servers…");
            let r = c.call("servers.audit", json!(null)).await?;
            for h in r["relays"].as_array().into_iter().flatten() {
                println!(
                    "  {}: {}",
                    h["url"].as_str().unwrap_or(""),
                    if h["reachable"] != json!(true) {
                        "unreachable".to_string()
                    } else if h["missing"].as_u64().unwrap_or(0) > 0 {
                        format!("was missing {} item(s), sent again", h["missing"])
                    } else {
                        format!("complete ({} items)", h["items"])
                    }
                );
            }
            println!(
                "{} item(s) sent again, {} refreshed, {} old chunk(s) removed.",
                r["resent"], r["refreshed"], r["removed_chunks"]
            );
        }
        Cmd::Send { id, to } => {
            let r = c.call("share.send", json!({"id": id, "to": to})).await?;
            println!(
                "Sent {} to {}.",
                r["name"].as_str().unwrap_or("the link"),
                r["to"].as_str().unwrap_or(&to)
            );
        }
        Cmd::Gallery {
            query,
            themes,
            plugins,
            setups,
            sort,
            limit,
        } => {
            let query = query.join(" ");
            if setups {
                let v = c.call("gallery.setups", json!({"query": query})).await?;
                if cli.json {
                    out(&v);
                    return Ok(());
                }
                let list = v.as_array().cloned().unwrap_or_default();
                if list.is_empty() {
                    println!("No setups yet. Publish yours: `peridot publish-setup \"My desk\"`.");
                }
                for s in list.iter().take(limit) {
                    let n = s["total"].as_u64().unwrap_or(0);
                    let have = s["installed"].as_u64().unwrap_or(0);
                    println!(
                        "{}  by {}{}\n  {}{}\n  {} of {} installed here · install: peridot install-setup {}",
                        s["title"].as_str().unwrap_or(""),
                        s["author"].as_str().unwrap_or(""),
                        if s["mine"] == json!(true) {
                            " (you)"
                        } else if s["following"] == json!(true) {
                            " (you follow them)"
                        } else {
                            ""
                        },
                        s["theme"]
                            .as_str()
                            .map(|t| format!("theme {t} · "))
                            .unwrap_or_default(),
                        s["summary"].as_str().unwrap_or(""),
                        have,
                        n,
                        s["coordinate"].as_str().unwrap_or("")
                    );
                }
                return Ok(());
            }
            let kind = if themes && !plugins {
                json!("theme")
            } else if plugins && !themes {
                json!("plugin")
            } else {
                Value::Null
            };
            let v = c
                .call(
                    "gallery.list",
                    json!({"kind": kind, "query": query, "sort": sort, "limit": limit}),
                )
                .await?;
            if cli.json {
                out(&v);
                return Ok(());
            }
            let items = v["items"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                let s = c.call("status", json!(null)).await?;
                let g = &s["gallery"];
                if g["themes"].as_u64().unwrap_or(0) + g["plugins"].as_u64().unwrap_or(0) == 0 {
                    println!("The catalogues haven't loaded yet; try again in a moment.");
                } else {
                    println!("Nothing matches.");
                }
            }
            for it in &items {
                let likes = it["likes"].as_u64().unwrap_or(0);
                let mut bits = vec![
                    format!("★ {}", it["stars"].as_u64().unwrap_or(0)),
                    format!("♥ {likes}"),
                ];
                if let Some(r) = it["rating"].as_f64() {
                    bits.push(format!(
                        "{r:.1}/5 from {} review(s)",
                        it["reviews"].as_u64().unwrap_or(0)
                    ));
                }
                if it["installed"] == json!(true) {
                    bits.push("installed".into());
                }
                if it["liked"] == json!(true) {
                    bits.push("you like this".into());
                }
                let by = it["liked_by"].as_array().cloned().unwrap_or_default();
                if !by.is_empty() {
                    bits.push(format!(
                        "liked by {}",
                        by.iter()
                            .filter_map(|n| n.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                println!(
                    "{} ({})  by {} · {}\n  {}\n  {}",
                    it["name"].as_str().unwrap_or(""),
                    it["kind"].as_str().unwrap_or(""),
                    it["author"].as_str().unwrap_or(""),
                    bits.join(" · "),
                    it["url"].as_str().unwrap_or(""),
                    it["description"].as_str().unwrap_or("")
                );
            }
            let total = v["total"].as_u64().unwrap_or(0);
            if total as usize > items.len() {
                println!(
                    "({} more; narrow it down with words, or --limit)",
                    total as usize - items.len()
                );
            }
        }
        Cmd::Like { url, undo } => {
            c.call("gallery.like", json!({"url": url, "on": !undo}))
                .await?;
            println!("{}", if undo { "Like taken back." } else { "Liked." });
        }
        Cmd::Review { url, text, rating } => {
            c.call(
                "gallery.review",
                json!({"url": url, "text": text, "rating": rating}),
            )
            .await?;
            println!("Review posted.");
        }
        Cmd::Install { url } => {
            let v = c.call("gallery.item", json!({"url": url})).await?;
            let kind = v["item"]["kind"].as_str().unwrap_or("plugin").to_string();
            println!(
                "Installing the {} {kind}…",
                v["item"]["name"].as_str().unwrap_or("")
            );
            c.call("gallery.install", json!({"url": url, "kind": kind}))
                .await?;
            println!("Done.");
        }
        Cmd::Follow { who, undo } => {
            let r = c.call("contacts.resolve", json!({"who": who})).await?;
            c.call(
                "gallery.follow",
                json!({"pubkey": r["pubkey"], "on": !undo}),
            )
            .await?;
            println!(
                "{} {}.",
                if undo { "Unfollowed" } else { "Following" },
                r["name"].as_str().unwrap_or(&who)
            );
        }
        Cmd::PublishSetup {
            title,
            summary,
            screenshot,
        } => {
            let screenshot = match screenshot {
                Some(p) => {
                    Some(std::fs::canonicalize(&p).with_context(|| format!("{}", p.display()))?)
                }
                None => None,
            };
            let mine = c.call("gallery.setup.mine", json!(null)).await?;
            if mine["can_publish"] != json!(true) {
                bail!("nothing to publish yet: no theme or plugin from git on this computer");
            }
            let r = c
                .call(
                    "gallery.setup.publish",
                    json!({"title": title, "summary": summary, "screenshot": screenshot}),
                )
                .await?;
            println!(
                "Published \"{}\": {} theme(s) and {} plugin(s){}.",
                r["title"].as_str().unwrap_or(""),
                r["themes"].as_array().map(Vec::len).unwrap_or(0),
                r["plugins"].as_array().map(Vec::len).unwrap_or(0),
                r["theme"]
                    .as_str()
                    .map(|t| format!(", theme {t}"))
                    .unwrap_or_default()
            );
        }
        Cmd::InstallSetup { coordinate } => {
            println!("Installing… (each theme and plugin is one Omarchy command)");
            let r = c
                .call("gallery.setup.install", json!({"coordinate": coordinate}))
                .await?;
            let done = r["done"].as_array().map(Vec::len).unwrap_or(0);
            let failed = r["failed"].as_array().cloned().unwrap_or_default();
            println!("{done} step(s) done, {} failed.", failed.len());
            for f in failed {
                println!("  {}: {}", f["step"], f["error"].as_str().unwrap_or(""));
            }
        }
        Cmd::Name { name } => {
            let p = c.call("profile.set", json!({"name": name})).await?;
            println!(
                "Others now see you as {}.",
                p["name"].as_str().unwrap_or(&name)
            );
        }
        Cmd::Opal { cmd: OpalCmd::Pair } => {
            println!("{OPAL_WILL_ASK}");
            c.call("opal.pair", json!(null)).await?;
            println!("Paired. Opal lists Peridot under Apps.");
        }
        Cmd::Shares => {
            let s = c.call("share.list", json!(null)).await?;
            let now = now_secs();
            for sh in s
                .as_array()
                .into_iter()
                .flatten()
                .filter(|s| s["revoked"] != json!(true))
            {
                let exp = sh["expires"].as_u64().unwrap_or(0);
                let left = if exp > now {
                    format!(
                        "{}d {}h left",
                        (exp - now) / 86400,
                        ((exp - now) % 86400) / 3600
                    )
                } else {
                    "expired".into()
                };
                println!(
                    "{:>4}  {:<32} {:>9}  {}\n      {}",
                    sh["id"],
                    sh["name"].as_str().unwrap_or(""),
                    human(sh["size"].as_u64().unwrap_or(0)),
                    left,
                    sh["url"].as_str().unwrap_or("")
                );
            }
        }
        Cmd::Unshare { id } => {
            c.call("share.revoke", json!({"id": id})).await?;
            println!("Removed. The link no longer opens.");
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
                    let health = s["servers"]["relays"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    for r in &relays {
                        let h = health
                            .iter()
                            .find(|h| h["url"].as_str() == Some(r.as_str()));
                        let note = match h {
                            None => String::new(),
                            Some(h) if h["reachable"] != json!(true) => {
                                "  · unreachable at the last check".into()
                            }
                            Some(h) if h["missing"].as_u64().unwrap_or(0) > 0 => {
                                format!("  · was missing {} item(s), sent again", h["missing"])
                            }
                            Some(h) => format!("  · complete ({} items)", h["items"]),
                        };
                        println!("{r}{note}");
                    }
                    if let Some(at) = s["servers"]["at"].as_u64() {
                        let mins = now_secs().saturating_sub(at) / 60;
                        println!(
                            "Last checked {}. `peridot tidy` checks now.",
                            if mins < 60 {
                                format!("{mins} min ago")
                            } else {
                                format!("{} h ago", mins / 60)
                            }
                        );
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

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn human(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / 1048576.0)
    }
}

/// Clipboard images go here before upload (inside home: the service can't
/// see /tmp).
fn share_tmp_dir() -> Result<PathBuf> {
    let dir = dirs_cache().join("share");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn dirs_cache() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
        })
        .join("peridot")
}

/// The newest image in the screenshot folder Omarchy uses.
fn last_screenshot() -> Option<PathBuf> {
    let dir = std::env::var_os("OMARCHY_SCREENSHOT_DIR")
        .or_else(|| std::env::var_os("XDG_PICTURES_DIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Pictures")
        });
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        let ext = p
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !["png", "jpg", "jpeg", "webp"].contains(&ext.as_str()) {
            continue;
        }
        let Ok(m) = e.metadata() else { continue };
        let Ok(t) = m.modified() else { continue };
        if best.as_ref().is_none_or(|(bt, _)| t > *bt) {
            best = Some((t, p));
        }
    }
    best.map(|(_, p)| p)
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
    let opal = &s["opal"];
    if opal["needs_pairing"] == json!(true) {
        println!("Pair with Opal to keep syncing: `peridot opal pair`");
    } else if opal["waiting_approval"] == json!(true) {
        println!("Waiting for your approval in Opal (in your bar).");
    }
    if let Some(n) = opal["shares_waiting"].as_u64().filter(|n| *n > 0) {
        println!("{n} expired link(s) will be removed once Opal allows it: `peridot shares`.");
    }
    if let Some(e) = s["error"].as_str() {
        println!("Problem: {e}");
    }
}

const OPAL_WILL_ASK: &str =
    "Opal will ask you to approve Peridot: look for its prompt in your bar.";
