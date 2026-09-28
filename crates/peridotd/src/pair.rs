//! Running a pairing: the new computer shows a code and waits; a computer
//! you already use takes the code, both show a number, you confirm on
//! both, and the sync secret moves across (the identity's own key only if
//! you ask). Each side uses its own short-lived relay connection with
//! one-time keys; see `peridot_sync::pairing` for the protocol.

use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use futures::StreamExt;
use nostr_sdk::prelude::*;
use opal_core::keystore::{ItemKind, SecretStore};
use peridot_sync::pairing::{CODE_LIFETIME, Joiner, JoinerStep, PairError, Sponsor, SponsorStep};
use serde::Serialize;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::app::App;

/// The keyring item holding this computer's permanent device key.
const DEVICE_KEY_ID: &str = "device";
/// How many pairings a computer may start from a typed code per window.
const JOIN_LIMIT: usize = 5;
const JOIN_WINDOW: u64 = 15 * 60;
/// After a No or an abort, wait this long before pairing again.
const COOLDOWN: u64 = 30;

#[derive(Debug, Clone, Default, Serialize)]
pub struct PairView {
    /// "new" on the computer joining, "existing" on the one sharing.
    pub role: &'static str,
    /// waiting | confirm (answer here) | waiting_other (you said yes; the
    /// other computer hasn't yet) | sending | approve (the new computer
    /// pairs with Opal) | done | expired | cancelled | aborted | failed
    pub stage: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number: Option<String>,
    /// The other computer's name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// On the sharing side: this computer holds the key, so it could hand
    /// it over too ("also keep the key on the new computer").
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub can_hold_key: bool,
    pub expires_at: u64,
}

/// Your answer to the number: does it match, and (sharing side only)
/// should the new computer also hold the key.
type Answer = (bool, bool);

pub struct PairSession {
    view: Arc<StdMutex<PairView>>,
    task: JoinHandle<()>,
    confirm: Option<oneshot::Sender<Answer>>,
}

impl PairSession {
    pub fn view(&self) -> PairView {
        self.view.lock().expect("not poisoned").clone()
    }
}

impl Drop for PairSession {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Keeps pairing from being hammered: a few starts per quarter hour, a
/// pause after a No, and never the same code twice.
#[derive(Default)]
pub struct PairLimiter {
    joins: Vec<u64>,
    cooldown_until: u64,
    used_codes: HashSet<String>,
}

impl PairLimiter {
    fn check_join(&mut self, code: &str, now: u64) -> anyhow::Result<()> {
        if now < self.cooldown_until {
            anyhow::bail!(
                "wait {} seconds before pairing again",
                self.cooldown_until - now
            );
        }
        self.joins.retain(|t| now.saturating_sub(*t) < JOIN_WINDOW);
        if self.joins.len() >= JOIN_LIMIT {
            anyhow::bail!("too many pairing attempts; try again in a few minutes");
        }
        let digest = peridot_sync::crypto::sha256_hex(code.trim().to_ascii_uppercase().as_bytes());
        if !self.used_codes.insert(digest) {
            anyhow::bail!("that code was already used; ask the new computer for a fresh one");
        }
        self.joins.push(now);
        Ok(())
    }

    pub fn cool_down(&mut self, now: u64) {
        self.cooldown_until = now + COOLDOWN;
    }
}

fn now() -> u64 {
    Timestamp::now().as_secs()
}

/// This computer's permanent device key: made once, kept in the keyring.
/// The sync secret is handed to it, so the identity's own key never has
/// to travel.
pub async fn device_key(store: &SecretStore) -> anyhow::Result<Keys> {
    if let Some(hex) = store.get(ItemKind::DeviceIdentity, DEVICE_KEY_ID).await? {
        return Ok(Keys::parse(hex.trim())?);
    }
    let keys = Keys::generate();
    store
        .put(
            ItemKind::DeviceIdentity,
            DEVICE_KEY_ID,
            "Peridot device key",
            &keys.secret_key().to_secret_hex(),
        )
        .await?;
    Ok(keys)
}

async fn pairing_client(app: &App) -> (Client, Vec<RelayUrl>) {
    let relays = opal_kit::relays::parse_urls(&app.config.read().await.relays);
    let client = Client::default();
    for r in &relays {
        let _ = client.add_relay(r).await;
    }
    client
        .connect()
        .and_wait(opal_kit::relays::CONNECT_WAIT)
        .await;
    (client, relays)
}

async fn subscribe(client: &Client, relays: &[RelayUrl], filter: Filter) -> anyhow::Result<()> {
    let filter = filter.since(Timestamp::from(now().saturating_sub(30)));
    let targets: Vec<(RelayUrl, Vec<Filter>)> = relays
        .iter()
        .map(|r| (r.clone(), vec![filter.clone()]))
        .collect();
    client.subscribe(targets).await?;
    Ok(())
}

async fn send(client: &Client, relays: &[RelayUrl], ev: &Event) -> anyhow::Result<()> {
    opal_kit::relays::publish(client, ev, relays)
        .await
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("couldn't reach the servers: {e}"))
}

fn set(view: &Arc<StdMutex<PairView>>, f: impl FnOnce(&mut PairView)) {
    f(&mut view.lock().expect("not poisoned"));
}

fn fail(view: &Arc<StdMutex<PairView>>, e: impl ToString) {
    set(view, |v| {
        v.stage = "failed";
        v.error = Some(e.to_string());
        v.code = None;
        v.qr = None;
    });
}

fn abort(view: &Arc<StdMutex<PairView>>, e: &PairError) {
    set(view, |v| {
        v.stage = "aborted";
        v.error = Some(e.to_string());
        v.code = None;
        v.qr = None;
        v.number = None;
    });
}

/// On the new computer: show a code and wait for a computer you already use.
pub async fn start_new(app: &Arc<App>) -> anyhow::Result<PairView> {
    anyhow::ensure!(!app.is_set_up().await, "this computer is already set up");
    let name = app.config.read().await.device_name();
    let device = device_key(&app.secrets).await?;
    let mut joiner = Joiner::new(&name, now(), device);
    let code = joiner.code();
    let view = Arc::new(StdMutex::new(PairView {
        role: "new",
        stage: "waiting",
        qr: opal_kit::qr::svg_data_url(&code).ok(),
        code: Some(code),
        expires_at: joiner.expires_at(),
        ..Default::default()
    }));
    let (client, relays) = pairing_client(app).await;
    // Listen before anything goes out: the channel only carries what arrives
    // after it's opened, and the other computer answers within a second.
    let mut notifications = client.notifications();
    subscribe(&client, &relays, joiner.filter()).await?;
    let (confirm_tx, mut confirm_rx) = oneshot::channel::<Answer>();

    let task_view = view.clone();
    let task_app = app.clone();
    let task = tokio::spawn(async move {
        let app = task_app;
        let view = task_view;
        let deadline = tokio::time::sleep(Duration::from_secs(CODE_LIFETIME));
        tokio::pin!(deadline);
        let mut waiting_for_confirm = false;
        loop {
            tokio::select! {
                n = notifications.next() => {
                    let Some(ClientNotification::Event { event, .. }) = n else {
                        if n.is_none() { break } else { continue }
                    };
                    match joiner.handle(&event, now()) {
                        Ok(JoinerStep::Reply { reply }) => {
                            if let Err(e) = send(&client, &relays, &reply).await {
                                fail(&view, e);
                                app.emit_state().await;
                                break;
                            }
                        }
                        Ok(JoinerStep::ShowNumber { number, sponsor }) => {
                            waiting_for_confirm = true;
                            set(&view, |v| { v.stage = "confirm"; v.number = Some(number); v.other = Some(sponsor); v.code = None; v.qr = None; });
                            app.emit_state().await;
                        }
                        Ok(JoinerStep::Paired { identity, reply }) => {
                            let _ = send(&client, &relays, &reply).await;
                            let result = async {
                                if identity.via_opal_mode() {
                                    if !app.opal.has_account(&identity.pubkey()).await {
                                        anyhow::bail!(
                                            "your other computer's identity is held by Opal. Set up Opal with the same key on this computer first, then pair again"
                                        );
                                    }
                                    // Opal here must let Peridot sign too.
                                    set(&view, |v| v.stage = "approve");
                                    app.emit_state().await;
                                    app.pair_opal(Some(identity.pubkey()))
                                        .await
                                        .map_err(|e| anyhow::anyhow!("Opal didn't pair with Peridot: {e}"))?;
                                }
                                identity.save(&app.secrets).await?;
                                app.start_engine(*identity, false).await
                            }.await;
                            match result {
                                Ok(()) => set(&view, |v| { v.stage = "done"; v.number = None; }),
                                Err(e) => fail(&view, e),
                            }
                            app.emit_state().await;
                            break;
                        }
                        Err(PairError::Expired) => break,
                        Err(e @ PairError::Aborted(_)) => {
                            abort(&view, &e);
                            app.pair_limits.lock().await.cool_down(now());
                            app.emit_state().await;
                            break;
                        }
                        // Noise or an impostor's guess: keep waiting.
                        Err(e) => tracing::warn!("pairing: ignored a message: {e}"),
                    }
                }
                answer = &mut confirm_rx, if waiting_for_confirm => {
                    waiting_for_confirm = false;
                    let yes = matches!(answer, Ok((true, _)));
                    match joiner.confirm(yes) {
                        Ok(Some(confirm)) => match send(&client, &relays, &confirm).await {
                            Ok(()) => set(&view, |v| v.stage = "waiting_other"),
                            Err(e) => { fail(&view, e); app.emit_state().await; break; }
                        },
                        Ok(None) => {
                            set(&view, |v| { v.stage = "cancelled"; v.number = None; });
                            app.pair_limits.lock().await.cool_down(now());
                            app.emit_state().await;
                            break;
                        }
                        Err(e) => { fail(&view, e); app.emit_state().await; break; }
                    }
                    app.emit_state().await;
                }
                _ = &mut deadline => break,
            }
        }
        {
            let mut v = view.lock().expect("not poisoned");
            if matches!(v.stage, "waiting" | "confirm" | "waiting_other") {
                v.stage = "expired";
                v.code = None;
                v.qr = None;
                v.number = None;
            }
        }
        app.emit_state().await;
        client.shutdown().await;
    });
    let out = view.lock().expect("not poisoned").clone();
    *app.pairing.lock().await = Some(PairSession {
        view,
        task,
        confirm: Some(confirm_tx),
    });
    app.emit_state().await;
    Ok(out)
}

/// On a computer you already use: take the code shown on the new one.
pub async fn start_existing(app: &Arc<App>, code: &str) -> anyhow::Result<PairView> {
    let engine = app.engine().await?;
    let identity = engine.identity().clone();
    if app.pairing.lock().await.as_ref().is_some_and(|s| {
        matches!(
            s.view().stage,
            "waiting" | "confirm" | "waiting_other" | "sending"
        )
    }) {
        anyhow::bail!("a pairing is already in progress; finish or cancel it first");
    }
    app.pair_limits.lock().await.check_join(code, now())?;
    let name = app.config.read().await.device_name();
    let (mut sponsor, hello) = Sponsor::start(code, &name, now())?;
    let view = Arc::new(StdMutex::new(PairView {
        role: "existing",
        stage: "waiting",
        can_hold_key: identity.keys.is_some(),
        expires_at: now() + CODE_LIFETIME,
        ..Default::default()
    }));
    let (client, relays) = pairing_client(app).await;
    // Listen before the hello goes out. Publishing waits on every relay
    // (one down, one wanting auth, and it takes seconds), while the new
    // computer replies within a second of hearing us: a channel opened
    // after the send would miss the reply and this side would wait forever.
    let mut notifications = client.notifications();
    subscribe(&client, &relays, sponsor.filter()).await?;
    send(&client, &relays, &hello).await?;
    let (confirm_tx, mut confirm_rx) = oneshot::channel::<Answer>();

    let task_view = view.clone();
    let task_app = app.clone();
    let task = tokio::spawn(async move {
        let app = task_app;
        let view = task_view;
        let deadline = tokio::time::sleep(Duration::from_secs(CODE_LIFETIME));
        tokio::pin!(deadline);
        let mut waiting_for_confirm = false;
        loop {
            tokio::select! {
                n = notifications.next() => {
                    let Some(ClientNotification::Event { event, .. }) = n else {
                        if n.is_none() { break } else { continue }
                    };
                    match sponsor.handle(&event, now()) {
                        Ok(SponsorStep::ShowNumber { reveal, number, joiner }) => {
                            if let Err(e) = send(&client, &relays, &reveal).await {
                                fail(&view, e);
                                app.emit_state().await;
                                break;
                            }
                            waiting_for_confirm = true;
                            set(&view, |v| { v.stage = "confirm"; v.number = Some(number); v.other = Some(joiner); });
                            app.emit_state().await;
                        }
                        Ok(SponsorStep::JoinerConfirmed { transfer }) => {
                            if let Some(transfer) = transfer {
                                match send(&client, &relays, &transfer).await {
                                    Ok(()) => set(&view, |v| v.stage = "sending"),
                                    Err(e) => { fail(&view, e); app.emit_state().await; break; }
                                }
                                app.emit_state().await;
                            }
                        }
                        Ok(SponsorStep::Done) => {
                            set(&view, |v| { v.stage = "done"; v.number = None; });
                            app.emit_state().await;
                            // The new computer announces itself shortly.
                            app.nudge.notify_one();
                            break;
                        }
                        Err(PairError::Expired) => break,
                        Err(e @ PairError::Aborted(_)) => {
                            abort(&view, &e);
                            app.pair_limits.lock().await.cool_down(now());
                            app.emit_state().await;
                            break;
                        }
                        Err(e) => tracing::warn!("pairing: ignored a message: {e}"),
                    }
                }
                answer = &mut confirm_rx, if waiting_for_confirm => {
                    waiting_for_confirm = false;
                    let (yes, hold_key) = answer.unwrap_or((false, false));
                    match sponsor.confirm(yes, &identity, hold_key) {
                        Ok(Some(transfer)) => match send(&client, &relays, &transfer).await {
                            Ok(()) => set(&view, |v| v.stage = "sending"),
                            Err(e) => { fail(&view, e); app.emit_state().await; break; }
                        },
                        Ok(None) if yes => set(&view, |v| v.stage = "waiting_other"),
                        Ok(None) => {
                            set(&view, |v| { v.stage = "cancelled"; v.number = None; });
                            app.pair_limits.lock().await.cool_down(now());
                            app.emit_state().await;
                            break;
                        }
                        Err(e) => { fail(&view, e); app.emit_state().await; break; }
                    }
                    app.emit_state().await;
                }
                _ = &mut deadline => break,
            }
        }
        {
            let mut v = view.lock().expect("not poisoned");
            if matches!(v.stage, "waiting" | "confirm" | "waiting_other") {
                v.stage = "expired";
                v.number = None;
            }
        }
        app.emit_state().await;
        client.shutdown().await;
    });
    let out = view.lock().expect("not poisoned").clone();
    *app.pairing.lock().await = Some(PairSession {
        view,
        task,
        confirm: Some(confirm_tx),
    });
    app.emit_state().await;
    Ok(out)
}

/// Your answer to "do both screens show the same number?", on either
/// computer. `hold_key` (sharing side) also hands the identity's key over.
pub async fn confirm(app: &Arc<App>, matches: bool, hold_key: bool) -> anyhow::Result<()> {
    let mut guard = app.pairing.lock().await;
    let session = guard
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("no pairing in progress"))?;
    if session.view().stage != "confirm" {
        anyhow::bail!("nothing to confirm right now");
    }
    let tx = session
        .confirm
        .take()
        .ok_or_else(|| anyhow::anyhow!("already answered"))?;
    let _ = tx.send((matches, hold_key));
    Ok(())
}

pub async fn cancel(app: &Arc<App>) {
    app.pairing.lock().await.take();
    app.emit_state().await;
}
