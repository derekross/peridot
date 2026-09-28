//! Pairing a new computer with one you already use.
//!
//! 1. The new computer ("joiner") shows a code: 80 random bits, typed as
//!    `PDT-XXXX-XXXX-XXXX-XXXX`. It listens at a meeting point derived from
//!    the code, and has a permanent device key `D`.
//! 2. On a computer you already use ("sponsor") you type the code. It says
//!    hello from a one-time key `S`, committing to a nonce it doesn't show
//!    yet.
//! 3. The joiner answers from its one-time key `E` with `D` and its own
//!    nonce. From here both sides share `K0`, derived from the code and
//!    the `S`/`E` Diffie-Hellman secret; every later message is
//!    authenticated with it.
//! 4. The sponsor reveals its nonce; the joiner checks the commitment. Both
//!    screens now show the same six digits, computed from everything said
//!    so far. Because each side fixed its randomness before seeing the
//!    other's, someone who glimpsed the code and jumped in between gets one
//!    guess in a million; there is nothing to grind.
//! 5. You confirm on **both** computers. Only then does the sponsor send
//!    the sync secret, encrypted to `D`. The identity's own key travels
//!    only when you tick "also keep the key on the new computer".
//!
//! Messages are ephemeral events (kind 21078): relays pass them on and
//! don't keep them. Codes expire after five minutes and work once.

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use crate::PAIR_KIND;
use crate::crypto::{SyncSecret, random_bytes};
use crate::identity::Identity;

/// How long a pairing code works.
pub const CODE_LIFETIME: u64 = 5 * 60;
/// The joiner must hear the reveal this soon after it replied.
const REVEAL_WINDOW: u64 = 60;
/// The sponsor must hear a reply this soon after its hello.
const REPLY_WINDOW: u64 = 120;
/// Messages that don't verify before a side gives up (relay noise is
/// cheap; a flood should be visible).
const MAX_BAD_MESSAGES: u32 = 20;
const CODE_BYTES: usize = 10;
/// Crockford's base32: no I, L, O or U to misread.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const VERSION: u8 = 2;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PairError {
    #[error("that code doesn't look right; check it and try again")]
    BadCode,
    #[error("that code has expired; start pairing again on the new computer")]
    Expired,
    #[error("unexpected pairing message")]
    Unexpected,
    #[error("the pairing message couldn't be verified")]
    Invalid,
    #[error("{0}")]
    Aborted(&'static str),
}

const ANOTHER: &str = "another computer answered this code; start again with a new one";
const NOISE: &str = "too many bad pairing messages arrived; start again with a new code";

/// The secret a pairing code carries.
#[derive(Clone)]
pub struct Code(Zeroizing<[u8; CODE_BYTES]>);

impl Code {
    pub fn generate() -> Self {
        Self(Zeroizing::new(random_bytes()))
    }

    /// `PDT-XXXX-XXXX-XXXX-XXXX`.
    pub fn display(&self) -> String {
        let mut bits: u128 = 0;
        for b in self.0.iter() {
            bits = (bits << 8) | u128::from(*b);
        }
        let mut chars = Vec::with_capacity(16);
        for i in (0..16).rev() {
            chars.push(ALPHABET[((bits >> (i * 5)) & 31) as usize] as char);
        }
        let groups: Vec<String> = chars.chunks(4).map(|c| c.iter().collect()).collect();
        format!("PDT-{}", groups.join("-"))
    }

    /// Accepts any case, spaces or dashes, and the usual misreadings
    /// (O for 0, I or L for 1).
    pub fn parse(input: &str) -> Result<Self, PairError> {
        let mut s: String = input
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .map(|c| c.to_ascii_uppercase())
            .collect();
        if let Some(rest) = s.strip_prefix("PDT") {
            s = rest.to_string();
        }
        if s.len() != 16 {
            return Err(PairError::BadCode);
        }
        let mut bits: u128 = 0;
        for c in s.chars() {
            let c = match c {
                'O' => '0',
                'I' | 'L' => '1',
                c => c,
            };
            let v = ALPHABET
                .iter()
                .position(|a| *a as char == c)
                .ok_or(PairError::BadCode)?;
            bits = (bits << 5) | v as u128;
        }
        let mut out = [0u8; CODE_BYTES];
        for (i, b) in out.iter_mut().enumerate() {
            *b = (bits >> (8 * (CODE_BYTES - 1 - i))) as u8;
        }
        bits.zeroize();
        Ok(Self(Zeroizing::new(out)))
    }

    /// The meeting point's keys: whoever knows the code can derive them.
    fn meeting_keys(&self) -> Keys {
        let hk = Hkdf::<Sha256>::new(Some(b"peridot/pair"), self.0.as_ref());
        let mut counter = 0u8;
        loop {
            let mut sk = Zeroizing::new([0u8; 32]);
            hk.expand(&[b"meeting".as_slice(), &[counter]].concat(), sk.as_mut())
                .expect("32 bytes is a valid length");
            if let Ok(secret) = SecretKey::from_slice(sk.as_ref()) {
                return Keys::new(secret);
            }
            counter += 1;
        }
    }

    /// The only thing the code itself authenticates: the hello, before
    /// there is a shared key.
    fn hello_mac(&self, sponsor: &PublicKey, commit: &[u8; 32]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.0.as_ref()).expect("any key length");
        mac.update(b"hello");
        mac.update(&sponsor.to_bytes());
        mac.update(commit);
        hex::encode(mac.finalize().into_bytes())
    }

    /// `K0`: from the code and the `S`/`E` shared secret, bound to both
    /// keys. Without the code, or without one of the two private keys,
    /// nothing after the hello can be read, forged or checked.
    fn session_key(
        &self,
        mine: &Keys,
        theirs: &PublicKey,
        sponsor: &PublicKey,
        joiner: &PublicKey,
    ) -> Result<Zeroizing<[u8; 32]>, PairError> {
        let shared = nip44::v2::ConversationKey::derive(mine.secret_key(), theirs)
            .map_err(|_| PairError::Invalid)?;
        let mut ikm = Zeroizing::new(Vec::with_capacity(32 + CODE_BYTES));
        ikm.extend_from_slice(shared.as_bytes());
        ikm.extend_from_slice(self.0.as_ref());
        let hk = Hkdf::<Sha256>::new(Some(b"peridot/pair/v2"), &ikm);
        let mut k0 = Zeroizing::new([0u8; 32]);
        hk.expand(
            &[&sponsor.to_bytes()[..], &joiner.to_bytes()[..]].concat(),
            k0.as_mut(),
        )
        .expect("32 bytes is a valid length");
        Ok(k0)
    }
}

fn commitment(sponsor: &PublicKey, nonce: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"peridot/pair/commit");
    h.update(sponsor.to_bytes());
    h.update(nonce);
    h.finalize().into()
}

/// Everything both sides said, in order.
#[allow(clippy::too_many_arguments)]
fn transcript(
    commit: &[u8; 32],
    sponsor: &PublicKey,
    joiner: &PublicKey,
    device: &PublicKey,
    n_s: &[u8; 32],
    n_e: &[u8; 32],
    name_s: &str,
    name_j: &str,
) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"peridot/pair/transcript");
    h.update(commit);
    h.update(sponsor.to_bytes());
    h.update(joiner.to_bytes());
    h.update(device.to_bytes());
    h.update(n_s);
    h.update(n_e);
    h.update((name_s.len() as u32).to_be_bytes());
    h.update(name_s.as_bytes());
    h.update((name_j.len() as u32).to_be_bytes());
    h.update(name_j.as_bytes());
    h.finalize().into()
}

/// The shared session: `K0` and the transcript, once both exist.
struct Session {
    k0: Zeroizing<[u8; 32]>,
    transcript: [u8; 32],
}

impl Session {
    fn mac(&self, label: &str, parts: &[&[u8]]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.k0.as_ref()).expect("any key length");
        mac.update(label.as_bytes());
        mac.update(&self.transcript);
        for p in parts {
            mac.update(p);
        }
        hex::encode(mac.finalize().into_bytes())
    }

    fn check(&self, label: &str, parts: &[&[u8]], given: &str) -> Result<(), PairError> {
        let want = self.mac(label, parts);
        // Constant time, so a wrong guess learns nothing from timing.
        let ok = want.len() == given.len()
            && want
                .bytes()
                .zip(given.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0;
        if ok { Ok(()) } else { Err(PairError::Invalid) }
    }

    /// Six digits both screens show.
    fn check_number(&self) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.k0.as_ref()).expect("any key length");
        mac.update(b"sas");
        mac.update(&self.transcript);
        let d = mac.finalize().into_bytes();
        let n = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) % 1_000_000;
        format!("{:03} {:03}", n / 1000, n % 1000)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum Message {
    Hello {
        v: u8,
        name: String,
        commit: String,
        mac: String,
    },
    Reply {
        v: u8,
        name: String,
        device: String,
        nonce: String,
        mac: String,
    },
    Reveal {
        v: u8,
        nonce: String,
        mac: String,
    },
    /// The person at the new computer said the numbers match.
    Confirm {
        v: u8,
        mac: String,
    },
    /// The sync secret, encrypted to the new computer's device key; the
    /// identity's own key only when you chose to hand it over.
    Transfer {
        v: u8,
        pubkey: String,
        wrap: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key_wrap: Option<String>,
        mac: String,
    },
    Done {
        v: u8,
        mac: String,
    },
}

impl Message {
    fn version(&self) -> u8 {
        match self {
            Message::Hello { v, .. }
            | Message::Reply { v, .. }
            | Message::Reveal { v, .. }
            | Message::Confirm { v, .. }
            | Message::Transfer { v, .. }
            | Message::Done { v, .. } => *v,
        }
    }
}

fn send(from: &Keys, to: &PublicKey, msg: &Message) -> anyhow::Result<Event> {
    let body = Zeroizing::new(serde_json::to_string(msg)?);
    let content = nip44::encrypt(from.secret_key(), to, body.as_str(), nip44::Version::V2)?;
    Ok(EventBuilder::new(Kind::Custom(PAIR_KIND), content)
        .tag(Tag::public_key(*to))
        .finalize(from)?)
}

fn receive(me: &Keys, ev: &Event) -> Result<Message, PairError> {
    if ev.kind != Kind::Custom(PAIR_KIND) || ev.verify().is_err() {
        return Err(PairError::Invalid);
    }
    let body = Zeroizing::new(
        nip44::decrypt(me.secret_key(), &ev.pubkey, &ev.content).map_err(|_| PairError::Invalid)?,
    );
    let msg: Message = serde_json::from_str(&body).map_err(|_| PairError::Invalid)?;
    // Older shapes are refused outright: no downgrade to a protocol that
    // could be ground.
    if msg.version() != VERSION {
        return Err(PairError::Invalid);
    }
    Ok(msg)
}

fn hex32(s: &str) -> Result<[u8; 32], PairError> {
    let v = hex::decode(s).map_err(|_| PairError::Invalid)?;
    v.try_into().map_err(|_| PairError::Invalid)
}

// ── The new computer ────────────────────────────────────────────────

enum JoinerState {
    /// Nothing heard yet.
    Waiting,
    /// Replied to a hello; waiting for the reveal.
    Replied {
        sponsor: PublicKey,
        sponsor_name: String,
        commit: [u8; 32],
        n_e: [u8; 32],
        replied_at: u64,
    },
    /// The number is on screen; waiting for the person here.
    ShowNumber {
        sponsor: PublicKey,
        session: Session,
    },
    /// The person here confirmed; waiting for the transfer.
    Confirmed {
        sponsor: PublicKey,
        session: Session,
    },
    /// Over, one way or another.
    Ended,
}

/// The new computer's side.
pub struct Joiner {
    code: Code,
    meeting: Keys,
    /// One-time key `E`.
    me: Option<Keys>,
    /// This computer's permanent device key `D`.
    device: Keys,
    name: String,
    created: u64,
    state: JoinerState,
    bad: u32,
}

/// What the joiner should do after a message.
pub enum JoinerStep {
    /// Send `reply`; nothing to show yet.
    Reply { reply: Event },
    /// Show `number` and the sponsor's name; ask the person here.
    ShowNumber { number: String, sponsor: String },
    /// Paired: save `identity`, send `reply`.
    Paired {
        identity: Box<Identity>,
        reply: Event,
    },
}

impl Joiner {
    /// `device` is this computer's permanent key, kept in the keyring.
    pub fn new(name: &str, now: u64, device: Keys) -> Self {
        let code = Code::generate();
        Self {
            meeting: code.meeting_keys(),
            code,
            me: Some(Keys::generate()),
            device,
            name: name.into(),
            created: now,
            state: JoinerState::Waiting,
            bad: 0,
        }
    }

    pub fn code(&self) -> String {
        self.code.display()
    }

    pub fn expires_at(&self) -> u64 {
        self.created + CODE_LIFETIME
    }

    /// What to listen for: messages to the meeting point or to us.
    pub fn filter(&self) -> Filter {
        let mut keys = vec![self.meeting.public_key()];
        if let Some(me) = &self.me {
            keys.push(me.public_key());
        }
        Filter::new().kind(Kind::Custom(PAIR_KIND)).pubkeys(keys)
    }

    fn end(&mut self) {
        self.state = JoinerState::Ended;
        // The one-time key is useless now; the code is single use.
        self.me = None;
        self.code.0.zeroize();
    }

    fn bad_message(&mut self) -> PairError {
        self.bad += 1;
        if self.bad >= MAX_BAD_MESSAGES {
            self.end();
            return PairError::Aborted(NOISE);
        }
        PairError::Invalid
    }

    pub fn handle(&mut self, ev: &Event, now: u64) -> Result<JoinerStep, PairError> {
        if matches!(self.state, JoinerState::Ended) {
            return Err(PairError::Unexpected);
        }
        if now > self.expires_at() {
            self.end();
            return Err(PairError::Expired);
        }
        let Some(me) = self.me.as_ref() else {
            return Err(PairError::Unexpected);
        };
        // A hello from a second computer, at any point after the first:
        // someone else has the code. Stop, visibly.
        let to_meeting = ev.tags.public_keys().next() == Some(self.meeting.public_key());
        if to_meeting && !matches!(self.state, JoinerState::Waiting) {
            if let Ok(Message::Hello { commit, mac, .. }) = receive(&self.meeting, ev)
                && let Ok(commit) = hex32(&commit)
                && mac == self.code.hello_mac(&ev.pubkey, &commit)
                && Some(ev.pubkey) != self.sponsor()
            {
                self.end();
                return Err(PairError::Aborted(ANOTHER));
            }
            return Err(self.bad_message());
        }
        match &self.state {
            JoinerState::Waiting => {
                let Ok(Message::Hello {
                    name, commit, mac, ..
                }) = receive(&self.meeting, ev)
                else {
                    return Err(self.bad_message());
                };
                let Ok(commit) = hex32(&commit) else {
                    return Err(self.bad_message());
                };
                if mac != self.code.hello_mac(&ev.pubkey, &commit) {
                    return Err(self.bad_message());
                }
                let n_e: [u8; 32] = random_bytes();
                let Ok(k0) = self
                    .code
                    .session_key(me, &ev.pubkey, &ev.pubkey, &me.public_key())
                else {
                    return Err(self.bad_message());
                };
                // The reply is authenticated with K0 over what it carries;
                // the transcript isn't complete yet, so it stands alone.
                let session = Session {
                    k0,
                    transcript: [0u8; 32],
                };
                let mac = session.mac(
                    "reply",
                    &[
                        &commit,
                        &self.device.public_key().to_bytes(),
                        &n_e,
                        self.name.as_bytes(),
                    ],
                );
                let reply = send(
                    me,
                    &ev.pubkey,
                    &Message::Reply {
                        v: VERSION,
                        name: self.name.clone(),
                        device: self.device.public_key().to_hex(),
                        nonce: hex::encode(n_e),
                        mac,
                    },
                )
                .map_err(|_| PairError::Invalid)?;
                self.state = JoinerState::Replied {
                    sponsor: ev.pubkey,
                    sponsor_name: clean_name(&name),
                    commit,
                    n_e,
                    replied_at: now,
                };
                Ok(JoinerStep::Reply { reply })
            }
            JoinerState::Replied {
                sponsor,
                sponsor_name,
                commit,
                n_e,
                replied_at,
            } => {
                let (sponsor, sponsor_name, commit, n_e, replied_at) =
                    (*sponsor, sponsor_name.clone(), *commit, *n_e, *replied_at);
                if ev.pubkey != sponsor {
                    return Err(self.bad_message());
                }
                if now > replied_at + REVEAL_WINDOW {
                    self.end();
                    return Err(PairError::Expired);
                }
                let Ok(Message::Reveal { nonce, mac, .. }) = receive(me, ev) else {
                    return Err(self.bad_message());
                };
                let Ok(n_s) = hex32(&nonce) else {
                    return Err(self.bad_message());
                };
                if commitment(&sponsor, &n_s) != commit {
                    // The sponsor can't change its mind after seeing E.
                    self.end();
                    return Err(PairError::Invalid);
                }
                let k0 = self
                    .code
                    .session_key(me, &sponsor, &sponsor, &me.public_key())?;
                let t = transcript(
                    &commit,
                    &sponsor,
                    &me.public_key(),
                    &self.device.public_key(),
                    &n_s,
                    &n_e,
                    &sponsor_name,
                    &self.name,
                );
                let session = Session { k0, transcript: t };
                if session.check("reveal", &[&n_s], &mac).is_err() {
                    self.end();
                    return Err(PairError::Invalid);
                }
                let number = session.check_number();
                self.state = JoinerState::ShowNumber { sponsor, session };
                Ok(JoinerStep::ShowNumber {
                    number,
                    sponsor: sponsor_name,
                })
            }
            JoinerState::ShowNumber { .. } => {
                // Nothing is accepted until the person here has answered.
                Err(self.bad_message())
            }
            JoinerState::Confirmed { sponsor, session } => {
                let sponsor = *sponsor;
                if ev.pubkey != sponsor {
                    return Err(self.bad_message());
                }
                let Ok(Message::Transfer {
                    pubkey,
                    wrap,
                    key_wrap,
                    mac,
                    ..
                }) = receive(me, ev)
                else {
                    return Err(self.bad_message());
                };
                let parts: Vec<&[u8]> = vec![
                    pubkey.as_bytes(),
                    wrap.as_bytes(),
                    key_wrap.as_deref().unwrap_or("").as_bytes(),
                ];
                if session.check("transfer", &parts, &mac).is_err() {
                    self.end();
                    return Err(PairError::Invalid);
                }
                let pubkey = PublicKey::from_hex(&pubkey).map_err(|_| PairError::Invalid)?;
                let open = |payload: &str| -> Result<Zeroizing<String>, PairError> {
                    nip44::decrypt(self.device.secret_key(), &sponsor, payload)
                        .map(Zeroizing::new)
                        .map_err(|_| PairError::Invalid)
                };
                let secret = SyncSecret::from_hex(&open(&wrap)?).map_err(|_| PairError::Invalid)?;
                let keys = match key_wrap {
                    Some(kw) => {
                        let keys =
                            Keys::parse(open(&kw)?.as_str()).map_err(|_| PairError::Invalid)?;
                        if keys.public_key() != pubkey {
                            return Err(PairError::Invalid);
                        }
                        Some(keys)
                    }
                    None => None,
                };
                let identity = Identity {
                    pubkey,
                    keys,
                    secret,
                    device: self.device.clone(),
                };
                let done_mac = session.mac("done", &[]);
                let reply = send(
                    me,
                    &sponsor,
                    &Message::Done {
                        v: VERSION,
                        mac: done_mac,
                    },
                )
                .map_err(|_| PairError::Invalid)?;
                self.end();
                Ok(JoinerStep::Paired {
                    identity: Box::new(identity),
                    reply,
                })
            }
            JoinerState::Ended => Err(PairError::Unexpected),
        }
    }

    /// The person here answered "do the numbers match?". Yes: the
    /// confirmation to send. No: the code is burnt.
    pub fn confirm(&mut self, matches: bool) -> Result<Option<Event>, PairError> {
        let JoinerState::ShowNumber { .. } = &self.state else {
            return Err(PairError::Unexpected);
        };
        if !matches {
            self.end();
            return Ok(None);
        }
        let JoinerState::ShowNumber { sponsor, session } =
            std::mem::replace(&mut self.state, JoinerState::Ended)
        else {
            unreachable!()
        };
        let me = self.me.as_ref().ok_or(PairError::Unexpected)?;
        let ev = send(
            me,
            &sponsor,
            &Message::Confirm {
                v: VERSION,
                mac: session.mac("confirm-j", &[]),
            },
        )
        .map_err(|_| PairError::Invalid)?;
        self.state = JoinerState::Confirmed { sponsor, session };
        Ok(Some(ev))
    }

    fn sponsor(&self) -> Option<PublicKey> {
        match &self.state {
            JoinerState::Replied { sponsor, .. }
            | JoinerState::ShowNumber { sponsor, .. }
            | JoinerState::Confirmed { sponsor, .. } => Some(*sponsor),
            _ => None,
        }
    }

    /// The device key this computer keeps after pairing.
    pub fn device(&self) -> &Keys {
        &self.device
    }
}

// ── A computer you already use ──────────────────────────────────────

/// What the sponsor hands over once both sides said yes.
struct Handover {
    pubkey: PublicKey,
    secret: Zeroizing<String>,
    key: Option<Zeroizing<String>>,
}

enum SponsorState {
    Started {
        n_s: [u8; 32],
        commit: [u8; 32],
        started: u64,
    },
    Revealed {
        joiner: PublicKey,
        device: PublicKey,
        session: Session,
        user_confirmed: Option<Handover>,
        joiner_confirmed: bool,
    },
    Sent {
        joiner: PublicKey,
        session: Session,
    },
    Ended,
}

/// The side of a computer you already use.
pub struct Sponsor {
    code: Code,
    /// One-time key `S`.
    me: Option<Keys>,
    name: String,
    state: SponsorState,
    bad: u32,
}

pub enum SponsorStep {
    /// Send `reveal`, show `number` and the new computer's name; ask
    /// whether it matches.
    ShowNumber {
        reveal: Event,
        number: String,
        joiner: String,
    },
    /// The new computer confirmed; if you already did, here is the
    /// transfer to send.
    JoinerConfirmed { transfer: Option<Event> },
    /// The new computer has everything.
    Done,
}

impl Sponsor {
    /// Start from a typed code; send the returned hello.
    pub fn start(code: &str, name: &str, now: u64) -> Result<(Self, Event), PairError> {
        let code = Code::parse(code)?;
        let me = Keys::generate();
        let n_s: [u8; 32] = random_bytes();
        let commit = commitment(&me.public_key(), &n_s);
        let hello = send(
            &me,
            &code.meeting_keys().public_key(),
            &Message::Hello {
                v: VERSION,
                name: name.into(),
                commit: hex::encode(commit),
                mac: code.hello_mac(&me.public_key(), &commit),
            },
        )
        .map_err(|_| PairError::Invalid)?;
        Ok((
            Self {
                code,
                me: Some(me),
                name: name.into(),
                state: SponsorState::Started {
                    n_s,
                    commit,
                    started: now,
                },
                bad: 0,
            },
            hello,
        ))
    }

    pub fn filter(&self) -> Filter {
        let mut f = Filter::new().kind(Kind::Custom(PAIR_KIND));
        if let Some(me) = &self.me {
            f = f.pubkey(me.public_key());
        }
        f
    }

    fn end(&mut self) {
        self.state = SponsorState::Ended;
        self.me = None;
        self.code.0.zeroize();
    }

    fn bad_message(&mut self) -> PairError {
        self.bad += 1;
        if self.bad >= MAX_BAD_MESSAGES {
            self.end();
            return PairError::Aborted(NOISE);
        }
        PairError::Invalid
    }

    pub fn handle(&mut self, ev: &Event, now: u64) -> Result<SponsorStep, PairError> {
        let Some(me) = self.me.as_ref() else {
            return Err(PairError::Unexpected);
        };
        match &self.state {
            SponsorState::Started {
                n_s,
                commit,
                started,
            } => {
                let (n_s, commit, started) = (*n_s, *commit, *started);
                if now > started + REPLY_WINDOW {
                    self.end();
                    return Err(PairError::Expired);
                }
                let Ok(Message::Reply {
                    name,
                    device,
                    nonce,
                    mac,
                    ..
                }) = receive(me, ev)
                else {
                    return Err(self.bad_message());
                };
                let (Ok(device_pk), Ok(n_e)) = (PublicKey::from_hex(&device), hex32(&nonce)) else {
                    return Err(self.bad_message());
                };
                let Ok(k0) = self
                    .code
                    .session_key(me, &ev.pubkey, &me.public_key(), &ev.pubkey)
                else {
                    return Err(self.bad_message());
                };
                let bare = Session {
                    k0,
                    transcript: [0u8; 32],
                };
                if bare
                    .check(
                        "reply",
                        &[&commit, &device_pk.to_bytes(), &n_e, name.as_bytes()],
                        &mac,
                    )
                    .is_err()
                {
                    return Err(self.bad_message());
                }
                let joiner_name = clean_name(&name);
                let t = transcript(
                    &commit,
                    &me.public_key(),
                    &ev.pubkey,
                    &device_pk,
                    &n_s,
                    &n_e,
                    &self.name,
                    &name,
                );
                let session = Session {
                    k0: bare.k0,
                    transcript: t,
                };
                let reveal = send(
                    me,
                    &ev.pubkey,
                    &Message::Reveal {
                        v: VERSION,
                        nonce: hex::encode(n_s),
                        mac: session.mac("reveal", &[&n_s]),
                    },
                )
                .map_err(|_| PairError::Invalid)?;
                let number = session.check_number();
                self.state = SponsorState::Revealed {
                    joiner: ev.pubkey,
                    device: device_pk,
                    session,
                    user_confirmed: None,
                    joiner_confirmed: false,
                };
                Ok(SponsorStep::ShowNumber {
                    reveal,
                    number,
                    joiner: joiner_name,
                })
            }
            SponsorState::Revealed { joiner, .. } => {
                let joiner = *joiner;
                if ev.pubkey != joiner {
                    // A second computer answering: someone else has the
                    // code (or the reply was replayed). Stop.
                    if let Ok(Message::Reply { .. }) = receive(me, ev) {
                        self.end();
                        return Err(PairError::Aborted(ANOTHER));
                    }
                    return Err(self.bad_message());
                }
                let Ok(Message::Confirm { mac, .. }) = receive(me, ev) else {
                    return Err(self.bad_message());
                };
                let SponsorState::Revealed { session, .. } = &self.state else {
                    unreachable!()
                };
                if session.check("confirm-j", &[], &mac).is_err() {
                    return Err(self.bad_message());
                }
                let SponsorState::Revealed {
                    joiner_confirmed, ..
                } = &mut self.state
                else {
                    unreachable!()
                };
                *joiner_confirmed = true;
                let transfer = self.transfer_if_ready()?;
                Ok(SponsorStep::JoinerConfirmed { transfer })
            }
            SponsorState::Sent { joiner, session } => {
                if ev.pubkey != *joiner {
                    return Err(self.bad_message());
                }
                let Ok(Message::Done { mac, .. }) = receive(me, ev) else {
                    return Err(self.bad_message());
                };
                if session.check("done", &[], &mac).is_err() {
                    return Err(self.bad_message());
                }
                self.end();
                Ok(SponsorStep::Done)
            }
            SponsorState::Ended => Err(PairError::Unexpected),
        }
    }

    /// You answered "do the numbers match?". `hold_key` also hands over
    /// the identity's own key (only possible when this computer holds it;
    /// off by default). The transfer goes out once the new computer has
    /// confirmed too: `Some` now, or later from [`Self::handle`].
    pub fn confirm(
        &mut self,
        matches: bool,
        identity: &Identity,
        hold_key: bool,
    ) -> Result<Option<Event>, PairError> {
        let SponsorState::Revealed { user_confirmed, .. } = &mut self.state else {
            return Err(PairError::Unexpected);
        };
        if user_confirmed.is_some() {
            return Err(PairError::Unexpected);
        }
        if !matches {
            self.end();
            return Ok(None);
        }
        *user_confirmed = Some(Handover {
            pubkey: identity.pubkey,
            secret: identity.secret.to_hex(),
            key: identity
                .keys
                .as_ref()
                .filter(|_| hold_key)
                .map(|k| Zeroizing::new(k.secret_key().to_secret_hex())),
        });
        self.transfer_if_ready()
    }

    fn transfer_if_ready(&mut self) -> Result<Option<Event>, PairError> {
        let SponsorState::Revealed {
            user_confirmed: Some(_),
            joiner_confirmed: true,
            ..
        } = &self.state
        else {
            return Ok(None);
        };
        let SponsorState::Revealed {
            joiner,
            device,
            session,
            user_confirmed: Some(h),
            ..
        } = std::mem::replace(&mut self.state, SponsorState::Ended)
        else {
            unreachable!()
        };
        let me = self.me.as_ref().ok_or(PairError::Unexpected)?;
        let wrap = nip44::encrypt(
            me.secret_key(),
            &device,
            h.secret.as_str(),
            nip44::Version::V2,
        )
        .map_err(|_| PairError::Invalid)?;
        let key_wrap = match &h.key {
            Some(k) => Some(
                nip44::encrypt(me.secret_key(), &device, k.as_str(), nip44::Version::V2)
                    .map_err(|_| PairError::Invalid)?,
            ),
            None => None,
        };
        let pubkey = h.pubkey.to_hex();
        let mac = session.mac(
            "transfer",
            &[
                pubkey.as_bytes(),
                wrap.as_bytes(),
                key_wrap.as_deref().unwrap_or("").as_bytes(),
            ],
        );
        let ev = send(
            me,
            &joiner,
            &Message::Transfer {
                v: VERSION,
                pubkey,
                wrap,
                key_wrap,
                mac,
            },
        )
        .map_err(|_| PairError::Invalid)?;
        self.state = SponsorState::Sent { joiner, session };
        Ok(Some(ev))
    }

    /// Both people have said yes.
    pub fn sent(&self) -> bool {
        matches!(self.state, SponsorState::Sent { .. })
    }
}

/// Names come from the other computer: one short line of plain text.
fn clean_name(name: &str) -> String {
    let flat: String = name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(40).collect();
    if out.is_empty() {
        out = "a computer".into();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joiner(name: &str, now: u64) -> Joiner {
        Joiner::new(name, now, Keys::generate())
    }

    /// Run the protocol up to both numbers being on screen.
    fn to_numbers(id_name: &str) -> (Joiner, Sponsor, String, String) {
        let mut j = joiner("New laptop", 1000);
        let (mut s, hello) = Sponsor::start(&j.code(), id_name, 1000).unwrap();
        let JoinerStep::Reply { reply } = j.handle(&hello, 1001).unwrap() else {
            panic!("expected a reply")
        };
        let SponsorStep::ShowNumber {
            reveal,
            number: n_s,
            joiner: jname,
        } = s.handle(&reply, 1001).unwrap()
        else {
            panic!("expected the number")
        };
        assert_eq!(jname, "New laptop");
        let JoinerStep::ShowNumber {
            number: n_j,
            sponsor: sname,
        } = j.handle(&reveal, 1002).unwrap()
        else {
            panic!("expected the number")
        };
        assert_eq!(sname, id_name);
        (j, s, n_j, n_s)
    }

    #[test]
    fn codes_round_trip_and_forgive_typos() {
        for _ in 0..50 {
            let code = Code::generate();
            let shown = code.display();
            assert_eq!(shown.len(), "PDT-XXXX-XXXX-XXXX-XXXX".len());
            assert_eq!(Code::parse(&shown).unwrap().0.as_ref(), code.0.as_ref());
            let sloppy = shown
                .to_lowercase()
                .replace('-', " ")
                .replace('0', "o")
                .replace('1', "l");
            assert_eq!(Code::parse(&sloppy).unwrap().0.as_ref(), code.0.as_ref());
        }
        assert_eq!(Code::parse("PDT-1234").err(), Some(PairError::BadCode));
        assert_eq!(
            Code::parse("PDT-UUUU-UUUU-UUUU-UUUU").err(),
            Some(PairError::BadCode)
        );
    }

    #[test]
    fn pairs_with_commit_reveal_and_both_confirm() {
        let id = Identity::generate();
        let (mut j, mut s, n_j, n_s) = to_numbers("Desk");
        assert_eq!(n_j, n_s);
        assert_eq!(n_j.len(), "000 000".len());

        // The sponsor says yes first: nothing goes out until the joiner does.
        assert!(s.confirm(true, &id, false).unwrap().is_none());
        assert!(!s.sent());
        let confirm = j.confirm(true).unwrap().expect("a confirmation to send");
        let SponsorStep::JoinerConfirmed {
            transfer: Some(transfer),
        } = s.handle(&confirm, 1003).unwrap()
        else {
            panic!("expected the transfer")
        };
        assert!(s.sent());
        assert!(!transfer.content.contains(id.secret.to_hex().as_str()));
        let JoinerStep::Paired { identity, reply } = j.handle(&transfer, 1004).unwrap() else {
            panic!("expected to be paired")
        };
        assert_eq!(identity.pubkey(), id.pubkey());
        assert_eq!(identity.secret.to_hex(), id.secret.to_hex());
        assert!(identity.keys.is_none(), "the key stays home by default");
        assert!(matches!(s.handle(&reply, 1005).unwrap(), SponsorStep::Done));
        // Single use.
        assert_eq!(j.handle(&transfer, 1006).err(), Some(PairError::Unexpected));
        assert_eq!(s.handle(&reply, 1006).err(), Some(PairError::Unexpected));
    }

    #[test]
    fn the_key_travels_only_when_asked_and_the_joiner_can_say_yes_first() {
        let id = Identity::generate();
        let (mut j, mut s, ..) = to_numbers("Desk");
        let confirm = j.confirm(true).unwrap().unwrap();
        let SponsorStep::JoinerConfirmed { transfer: None } = s.handle(&confirm, 1003).unwrap()
        else {
            panic!("the sponsor hasn't said yes yet")
        };
        let transfer = s.confirm(true, &id, true).unwrap().expect("both said yes");
        let JoinerStep::Paired { identity, .. } = j.handle(&transfer, 1004).unwrap() else {
            panic!()
        };
        assert_eq!(
            identity.keys.as_ref().map(|k| k.public_key()),
            Some(id.pubkey())
        );
    }

    #[test]
    fn an_opal_held_identity_pairs_with_the_wrap_only() {
        let id = Identity::via_opal(Keys::generate().public_key());
        let (mut j, mut s, ..) = to_numbers("Desk");
        let confirm = j.confirm(true).unwrap().unwrap();
        s.handle(&confirm, 1003).unwrap();
        // Asking to hold the key changes nothing when there is none here.
        let transfer = s.confirm(true, &id, true).unwrap().unwrap();
        let JoinerStep::Paired { identity, .. } = j.handle(&transfer, 1004).unwrap() else {
            panic!()
        };
        assert!(identity.via_opal_mode());
        assert_eq!(identity.secret.to_hex(), id.secret.to_hex());
    }

    #[test]
    fn a_second_hello_aborts_instead_of_racing() {
        let mut j = joiner("New", 0);
        let code = j.code();
        let (_, hello1) = Sponsor::start(&code, "Desk", 0).unwrap();
        let (_, hello2) = Sponsor::start(&code, "Desk", 0).unwrap();
        assert!(matches!(j.handle(&hello1, 1), Ok(JoinerStep::Reply { .. })));
        assert_eq!(
            j.handle(&hello2, 2).err(),
            Some(PairError::Aborted(ANOTHER))
        );
        // And the code is dead.
        assert_eq!(j.handle(&hello1, 3).err(), Some(PairError::Unexpected));
    }

    #[test]
    fn a_second_reply_aborts_the_sponsor() {
        let mut j1 = joiner("New", 0);
        let (mut s, hello) = Sponsor::start(&j1.code(), "Desk", 0).unwrap();
        let JoinerStep::Reply { reply } = j1.handle(&hello, 1).unwrap() else {
            panic!()
        };
        s.handle(&reply, 1).unwrap();
        // Someone else who saw the code replies too (with a valid MAC,
        // since they know the code and can do the Diffie-Hellman).
        let mut j2 = Joiner {
            code: Code::parse(&j1.code()).unwrap(),
            meeting: Code::parse(&j1.code()).unwrap().meeting_keys(),
            me: Some(Keys::generate()),
            device: Keys::generate(),
            name: "Evil".into(),
            created: 0,
            state: JoinerState::Waiting,
            bad: 0,
        };
        let JoinerStep::Reply { reply: reply2 } = j2.handle(&hello, 1).unwrap() else {
            panic!()
        };
        assert_eq!(
            s.handle(&reply2, 2).err(),
            Some(PairError::Aborted(ANOTHER))
        );
    }

    #[test]
    fn nothing_can_be_ground() {
        // The joiner shows its number only after the reveal opened the
        // commitment, and it accepts exactly one reply and one reveal.
        let (mut j, mut s, ..) = to_numbers("Desk");
        let (_, other_hello) = Sponsor::start(&Code::generate().display(), "X", 0).unwrap();
        assert!(j.handle(&other_hello, 1003).is_err());
        // Before the person answers, nothing else is taken.
        let id = Identity::generate();
        s.confirm(true, &id, false).unwrap();
        let fake = Keys::generate();
        let bogus = send(
            &fake,
            &j.me.as_ref().unwrap().public_key(),
            &Message::Done {
                v: VERSION,
                mac: "00".into(),
            },
        )
        .unwrap();
        assert_eq!(j.handle(&bogus, 1003).err(), Some(PairError::Invalid));
    }

    #[test]
    fn a_reveal_that_doesnt_open_the_commit_is_rejected() {
        let mut j = joiner("New", 0);
        let (s, hello) = Sponsor::start(&j.code(), "Desk", 0).unwrap();
        let JoinerStep::Reply { reply: _ } = j.handle(&hello, 1).unwrap() else {
            panic!()
        };
        let sponsor_keys = s.me.as_ref().unwrap().clone();
        // A reveal with a different nonce than committed.
        let bad = send(
            &sponsor_keys,
            &j.me.as_ref().unwrap().public_key(),
            &Message::Reveal {
                v: VERSION,
                nonce: hex::encode([9u8; 32]),
                mac: "00".into(),
            },
        )
        .unwrap();
        assert_eq!(j.handle(&bad, 2).err(), Some(PairError::Invalid));
        assert!(matches!(j.state, JoinerState::Ended));
    }

    #[test]
    fn transfer_needs_both_confirmations_and_is_bound_to_the_transcript() {
        let id = Identity::generate();
        let (mut j, mut s, ..) = to_numbers("Desk");
        // The joiner hasn't confirmed: a transfer arriving now is refused.
        let sponsor_keys = s.me.as_ref().unwrap().clone();
        let joiner_pk = j.me.as_ref().unwrap().public_key();
        let early = send(
            &sponsor_keys,
            &joiner_pk,
            &Message::Transfer {
                v: VERSION,
                pubkey: id.pubkey.to_hex(),
                wrap: "x".into(),
                key_wrap: None,
                mac: "00".into(),
            },
        )
        .unwrap();
        assert_eq!(j.handle(&early, 1003).err(), Some(PairError::Invalid));
        let confirm = j.confirm(true).unwrap().unwrap();
        s.handle(&confirm, 1003).unwrap();
        let transfer = s.confirm(true, &id, false).unwrap().unwrap();
        // Tampering with the wrap breaks the MAC.
        let Message::Transfer {
            pubkey,
            key_wrap,
            mac,
            ..
        } = receive(j.me.as_ref().unwrap(), &transfer).unwrap()
        else {
            panic!()
        };
        let tampered = send(
            &sponsor_keys,
            &joiner_pk,
            &Message::Transfer {
                v: VERSION,
                pubkey,
                wrap: nip44::encrypt(
                    sponsor_keys.secret_key(),
                    &j.device.public_key(),
                    SyncSecret::generate().to_hex().as_str(),
                    nip44::Version::V2,
                )
                .unwrap(),
                key_wrap,
                mac,
            },
        )
        .unwrap();
        assert_eq!(j.handle(&tampered, 1004).err(), Some(PairError::Invalid));
    }

    #[test]
    fn declining_burns_the_code_on_either_side() {
        let id = Identity::generate();
        let (mut j, mut s, ..) = to_numbers("Desk");
        assert!(j.confirm(false).unwrap().is_none());
        assert_eq!(j.confirm(true).err(), Some(PairError::Unexpected));
        assert!(s.confirm(false, &id, false).unwrap().is_none());
        assert_eq!(
            s.confirm(true, &id, false).err(),
            Some(PairError::Unexpected)
        );
    }

    #[test]
    fn expiry_wrong_code_and_v1_messages_fail() {
        let mut j = joiner("New", 0);
        let (_, hello) = Sponsor::start(&Code::generate().display(), "Desk", 0).unwrap();
        assert_eq!(j.handle(&hello, 1).err(), Some(PairError::Invalid));
        let (_, hello) = Sponsor::start(&j.code(), "Desk", 0).unwrap();
        assert_eq!(
            j.handle(&hello, CODE_LIFETIME + 1).err(),
            Some(PairError::Expired)
        );
        // The sponsor gives up when no reply comes in time.
        let mut j = joiner("New", 0);
        let (mut s, hello) = Sponsor::start(&j.code(), "Desk", 0).unwrap();
        let JoinerStep::Reply { reply } = j.handle(&hello, 1).unwrap() else {
            panic!()
        };
        assert_eq!(
            s.handle(&reply, REPLY_WINDOW + 1).err(),
            Some(PairError::Expired)
        );
        // The joiner gives up when the reveal is late.
        let mut j = joiner("New", 0);
        let (mut s, hello) = Sponsor::start(&j.code(), "Desk", 0).unwrap();
        let JoinerStep::Reply { reply } = j.handle(&hello, 1).unwrap() else {
            panic!()
        };
        let SponsorStep::ShowNumber { reveal, .. } = s.handle(&reply, 2).unwrap() else {
            panic!()
        };
        assert_eq!(
            j.handle(&reveal, 1 + REVEAL_WINDOW + 1).err(),
            Some(PairError::Expired)
        );
        // A message in the old shape (no version) is refused.
        let mut j = joiner("New", 0);
        let code = Code::parse(&j.code()).unwrap();
        let s_keys = Keys::generate();
        let old = nip44::encrypt(
            s_keys.secret_key(),
            &code.meeting_keys().public_key(),
            r#"{"t":"hello","name":"Desk","mac":"00"}"#,
            nip44::Version::V2,
        )
        .unwrap();
        let ev = EventBuilder::new(Kind::Custom(PAIR_KIND), old)
            .tag(Tag::public_key(code.meeting_keys().public_key()))
            .finalize(&s_keys)
            .unwrap();
        assert_eq!(j.handle(&ev, 1).err(), Some(PairError::Invalid));
        assert_eq!(
            clean_name("\u{1b}[31mEvil\nname  that is much much much longer than forty chars"),
            "[31mEvil name that is much much much lon"
        );
    }

    #[test]
    fn a_flood_of_bad_messages_ends_it() {
        let mut j = joiner("New", 0);
        let noise = Keys::generate();
        let mut last = None;
        for i in 0..MAX_BAD_MESSAGES {
            let ev = send(
                &noise,
                &j.meeting.public_key(),
                &Message::Done {
                    v: VERSION,
                    mac: format!("{i}"),
                },
            )
            .unwrap();
            last = j.handle(&ev, 1).err();
        }
        assert_eq!(last, Some(PairError::Aborted(NOISE)));
    }
}
