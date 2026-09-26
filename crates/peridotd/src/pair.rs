//! Running a pairing: the new computer shows a code and waits; a computer
//! you already use takes the code, both show a number, you confirm, and
//! your identity moves across. Each side uses its own short-lived relay
//! connection with one-time keys.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use futures::StreamExt;
use nostr_sdk::prelude::*;
use peridot_sync::pairing::{CODE_LIFETIME, Joiner, JoinerStep, PairError, Sponsor, SponsorStep};
use serde::Serialize;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::app::App;

#[derive(Debug, Clone, Default, Serialize)]
pub struct PairView {
    /// "new" on the computer joining, "existing" on the one sharing.
    pub role: &'static str,
    /// waiting | confirm | approve (the new computer pairs with Opal) |
    /// sending | done | expired | cancelled | failed
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
    pub expires_at: u64,
}

pub struct PairSession {
    view: Arc<StdMutex<PairView>>,
    task: JoinHandle<()>,
    confirm: Option<oneshot::Sender<bool>>,
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

fn now() -> u64 {
    Timestamp::now().as_secs()
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

/// On the new computer: show a code and wait for a computer you already use.
pub async fn start_new(app: &Arc<App>) -> anyhow::Result<PairView> {
    anyhow::ensure!(!app.is_set_up().await, "this computer is already set up");
    let name = app.config.read().await.device_name();
    let mut joiner = Joiner::new(&name, now());
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
    subscribe(&client, &relays, joiner.filter()).await?;

    let task_view = view.clone();
    let task_app = app.clone();
    let task = tokio::spawn(async move {
        let app = task_app;
        let view = task_view;
        let mut notifications = client.notifications();
        let deadline = tokio::time::sleep(Duration::from_secs(CODE_LIFETIME));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                n = notifications.next() => {
                    let Some(ClientNotification::Event { event, .. }) = n else {
                        if n.is_none() { break } else { continue }
                    };
                    match joiner.handle(&event, now()) {
                        Ok(JoinerStep::ShowNumber { reply, number, sponsor }) => {
                            if let Err(e) = send(&client, &relays, &reply).await {
                                set(&view, |v| { v.stage = "failed"; v.error = Some(e.to_string()); });
                            } else {
                                set(&view, |v| { v.stage = "confirm"; v.number = Some(number); v.other = Some(sponsor); });
                            }
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
                            set(&view, |v| match result {
                                Ok(()) => { v.stage = "done"; v.code = None; v.qr = None; }
                                Err(e) => { v.stage = "failed"; v.error = Some(e.to_string()); }
                            });
                            app.emit_state().await;
                            break;
                        }
                        Err(PairError::Expired) => break,
                        // Noise or an impostor: keep waiting.
                        Err(e) => tracing::debug!("pairing: {e}"),
                    }
                }
                _ = &mut deadline => break,
            }
        }
        {
            let mut v = view.lock().expect("not poisoned");
            if v.stage == "waiting" || v.stage == "confirm" {
                v.stage = "expired";
                v.code = None;
                v.qr = None;
            }
        }
        app.emit_state().await;
        client.shutdown().await;
    });
    let out = view.lock().expect("not poisoned").clone();
    *app.pairing.lock().await = Some(PairSession {
        view,
        task,
        confirm: None,
    });
    app.emit_state().await;
    Ok(out)
}

/// On a computer you already use: take the code shown on the new one.
pub async fn start_existing(app: &Arc<App>, code: &str) -> anyhow::Result<PairView> {
    let engine = app.engine().await?;
    let identity = engine.identity().clone();
    let name = app.config.read().await.device_name();
    let (mut sponsor, hello) = Sponsor::start(code, &name)?;
    let view = Arc::new(StdMutex::new(PairView {
        role: "existing",
        stage: "waiting",
        expires_at: now() + CODE_LIFETIME,
        ..Default::default()
    }));
    let (client, relays) = pairing_client(app).await;
    subscribe(&client, &relays, sponsor.filter()).await?;
    send(&client, &relays, &hello).await?;
    let (confirm_tx, mut confirm_rx) = oneshot::channel::<bool>();

    let task_view = view.clone();
    let task_app = app.clone();
    let task = tokio::spawn(async move {
        let app = task_app;
        let view = task_view;
        let mut notifications = client.notifications();
        let deadline = tokio::time::sleep(Duration::from_secs(CODE_LIFETIME));
        tokio::pin!(deadline);
        let mut waiting_for_confirm = false;
        loop {
            tokio::select! {
                n = notifications.next() => {
                    let Some(ClientNotification::Event { event, .. }) = n else {
                        if n.is_none() { break } else { continue }
                    };
                    match sponsor.handle(&event) {
                        Ok(SponsorStep::ShowNumber { number, joiner }) => {
                            waiting_for_confirm = true;
                            set(&view, |v| { v.stage = "confirm"; v.number = Some(number); v.other = Some(joiner); });
                            app.emit_state().await;
                        }
                        Ok(SponsorStep::Done) => {
                            set(&view, |v| v.stage = "done");
                            app.emit_state().await;
                            // The new computer announces itself shortly.
                            app.nudge.notify_one();
                            break;
                        }
                        Err(e) => tracing::debug!("pairing: {e}"),
                    }
                }
                ok = &mut confirm_rx, if waiting_for_confirm => {
                    waiting_for_confirm = false;
                    if ok != Ok(true) {
                        set(&view, |v| v.stage = "cancelled");
                        app.emit_state().await;
                        break;
                    }
                    match sponsor.confirm(&identity) {
                        Ok(transfer) => match send(&client, &relays, &transfer).await {
                            Ok(()) => set(&view, |v| v.stage = "sending"),
                            Err(e) => set(&view, |v| { v.stage = "failed"; v.error = Some(e.to_string()); }),
                        },
                        Err(e) => set(&view, |v| { v.stage = "failed"; v.error = Some(e.to_string()); }),
                    }
                    app.emit_state().await;
                }
                _ = &mut deadline => break,
            }
        }
        {
            let mut v = view.lock().expect("not poisoned");
            if v.stage == "waiting" || v.stage == "confirm" {
                v.stage = "expired";
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

/// Your answer to "do both screens show the same number?".
pub async fn confirm(app: &Arc<App>, matches: bool) -> anyhow::Result<()> {
    let mut guard = app.pairing.lock().await;
    let session = guard
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("no pairing in progress"))?;
    let tx = session
        .confirm
        .take()
        .ok_or_else(|| anyhow::anyhow!("nothing to confirm on this computer"))?;
    let _ = tx.send(matches);
    Ok(())
}

pub async fn cancel(app: &Arc<App>) {
    app.pairing.lock().await.take();
    app.emit_state().await;
}
