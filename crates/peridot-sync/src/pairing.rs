//! Pairing a new computer with one you already use.
//!
//! 1. The new computer ("joiner") shows a code: 80 random bits, typed as
//!    `PDT-XXXX-XXXX-XXXX-XXXX`. It listens at a meeting point derived from
//!    the code.
//! 2. On a computer you already use ("sponsor") you type the code. It says
//!    hello from a one-time key, to the meeting point.
//! 3. The joiner answers from its own one-time key. Both screens now show
//!    the same six digits, computed from the code and both one-time keys.
//!    Someone who glimpsed the code and jumped in between would make the
//!    numbers differ, so you only confirm when they match.
//! 4. The sponsor sends your identity, encrypted to the joiner's one-time
//!    key, and the joiner says it's done.
//!
//! Messages are ephemeral events (kind 21078): relays pass them on and
//! don't keep them. Codes expire after five minutes.

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
const CODE_BYTES: usize = 10;
/// Crockford's base32: no I, L, O or U to misread.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

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
}

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

    fn mac(&self, label: &str, parts: &[&PublicKey]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.0.as_ref()).expect("any key length");
        mac.update(label.as_bytes());
        for p in parts {
            mac.update(&p.to_bytes());
        }
        hex::encode(mac.finalize().into_bytes())
    }

    /// Six digits both screens show.
    fn check_number(&self, sponsor: &PublicKey, joiner: &PublicKey) -> String {
        let mut h = Sha256::new();
        h.update(b"peridot/pair/check");
        h.update(self.0.as_ref());
        h.update(sponsor.to_bytes());
        h.update(joiner.to_bytes());
        let d = h.finalize();
        let n = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) % 1_000_000;
        format!("{:03} {:03}", n / 1000, n % 1000)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum Message {
    Hello {
        name: String,
        mac: String,
    },
    Reply {
        name: String,
        mac: String,
    },
    /// The identity: its key when this computer holds it, or just the
    /// public key when Opal does (the new computer needs Opal too).
    Transfer {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        pubkey: String,
        sync_secret: String,
    },
    Done,
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
    serde_json::from_str(&body).map_err(|_| PairError::Invalid)
}

/// The new computer's side.
pub struct Joiner {
    code: Code,
    meeting: Keys,
    me: Keys,
    name: String,
    created: u64,
    sponsor: Option<PublicKey>,
    done: bool,
}

/// What the joiner should do after a message.
pub enum JoinerStep {
    /// Send `reply` and show `number` (and the sponsor's name).
    ShowNumber {
        reply: Event,
        number: String,
        sponsor: String,
    },
    /// Paired: save `identity`, send `reply`.
    Paired {
        identity: Box<Identity>,
        reply: Event,
    },
}

impl Joiner {
    pub fn new(name: &str, now: u64) -> Self {
        let code = Code::generate();
        Self {
            meeting: code.meeting_keys(),
            code,
            me: Keys::generate(),
            name: name.into(),
            created: now,
            sponsor: None,
            done: false,
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
        Filter::new()
            .kind(Kind::Custom(PAIR_KIND))
            .pubkeys([self.meeting.public_key(), self.me.public_key()])
    }

    pub fn handle(&mut self, ev: &Event, now: u64) -> Result<JoinerStep, PairError> {
        if self.done {
            return Err(PairError::Unexpected);
        }
        if now > self.expires_at() {
            return Err(PairError::Expired);
        }
        match self.sponsor {
            None => {
                // First hello wins; the check number exposes an impostor.
                let Message::Hello { name, mac } = receive(&self.meeting, ev)? else {
                    return Err(PairError::Unexpected);
                };
                if mac != self.code.mac("hello", &[&ev.pubkey]) {
                    return Err(PairError::Invalid);
                }
                self.sponsor = Some(ev.pubkey);
                let reply = send(
                    &self.me,
                    &ev.pubkey,
                    &Message::Reply {
                        name: self.name.clone(),
                        mac: self.code.mac("reply", &[&ev.pubkey, &self.me.public_key()]),
                    },
                )
                .map_err(|_| PairError::Invalid)?;
                Ok(JoinerStep::ShowNumber {
                    reply,
                    number: self.code.check_number(&ev.pubkey, &self.me.public_key()),
                    sponsor: clean_name(&name),
                })
            }
            Some(sponsor) => {
                if ev.pubkey != sponsor {
                    return Err(PairError::Unexpected);
                }
                let Message::Transfer {
                    key,
                    pubkey,
                    sync_secret,
                } = receive(&self.me, ev)?
                else {
                    return Err(PairError::Unexpected);
                };
                let pubkey = PublicKey::from_hex(&pubkey).map_err(|_| PairError::Invalid)?;
                let keys = match key {
                    Some(k) => {
                        let keys = Keys::parse(&k).map_err(|_| PairError::Invalid)?;
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
                    secret: SyncSecret::from_hex(&sync_secret).map_err(|_| PairError::Invalid)?,
                };
                self.done = true;
                let reply =
                    send(&self.me, &sponsor, &Message::Done).map_err(|_| PairError::Invalid)?;
                Ok(JoinerStep::Paired {
                    identity: Box::new(identity),
                    reply,
                })
            }
        }
    }
}

/// The side of a computer you already use.
pub struct Sponsor {
    code: Code,
    me: Keys,
    joiner: Option<PublicKey>,
    confirmed: bool,
    done: bool,
}

pub enum SponsorStep {
    /// Show `number` and the new computer's name; ask whether it matches.
    ShowNumber { number: String, joiner: String },
    /// The new computer has everything.
    Done,
}

impl Sponsor {
    /// Start from a typed code; send the returned hello.
    pub fn start(code: &str, name: &str) -> Result<(Self, Event), PairError> {
        let code = Code::parse(code)?;
        let me = Keys::generate();
        let hello = send(
            &me,
            &code.meeting_keys().public_key(),
            &Message::Hello {
                name: name.into(),
                mac: code.mac("hello", &[&me.public_key()]),
            },
        )
        .map_err(|_| PairError::Invalid)?;
        Ok((
            Self {
                code,
                me,
                joiner: None,
                confirmed: false,
                done: false,
            },
            hello,
        ))
    }

    pub fn filter(&self) -> Filter {
        Filter::new()
            .kind(Kind::Custom(PAIR_KIND))
            .pubkey(self.me.public_key())
    }

    pub fn handle(&mut self, ev: &Event) -> Result<SponsorStep, PairError> {
        if self.done {
            return Err(PairError::Unexpected);
        }
        match self.joiner {
            None => {
                let Message::Reply { name, mac } = receive(&self.me, ev)? else {
                    return Err(PairError::Unexpected);
                };
                if mac != self.code.mac("reply", &[&self.me.public_key(), &ev.pubkey]) {
                    return Err(PairError::Invalid);
                }
                self.joiner = Some(ev.pubkey);
                Ok(SponsorStep::ShowNumber {
                    number: self.code.check_number(&self.me.public_key(), &ev.pubkey),
                    joiner: clean_name(&name),
                })
            }
            Some(joiner) => {
                if ev.pubkey != joiner || !self.confirmed {
                    return Err(PairError::Unexpected);
                }
                let Message::Done = receive(&self.me, ev)? else {
                    return Err(PairError::Unexpected);
                };
                self.done = true;
                Ok(SponsorStep::Done)
            }
        }
    }

    /// You confirmed the numbers match: the identity to send.
    pub fn confirm(&mut self, identity: &Identity) -> Result<Event, PairError> {
        let joiner = self.joiner.ok_or(PairError::Unexpected)?;
        if self.confirmed {
            return Err(PairError::Unexpected);
        }
        self.confirmed = true;
        send(
            &self.me,
            &joiner,
            &Message::Transfer {
                key: identity
                    .keys
                    .as_ref()
                    .map(|k| k.secret_key().to_secret_hex()),
                pubkey: identity.pubkey.to_hex(),
                sync_secret: identity.secret.to_hex(),
            },
        )
        .map_err(|_| PairError::Invalid)
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
    fn pairs_and_both_sides_see_the_same_number() {
        let id = Identity::generate();
        let mut joiner = Joiner::new("New laptop", 1000);
        let (mut sponsor, hello) = Sponsor::start(&joiner.code(), "Desk").unwrap();

        let JoinerStep::ShowNumber {
            reply,
            number: n1,
            sponsor: sname,
        } = joiner.handle(&hello, 1001).unwrap()
        else {
            panic!()
        };
        assert_eq!(sname, "Desk");
        let SponsorStep::ShowNumber {
            number: n2,
            joiner: jname,
        } = sponsor.handle(&reply).unwrap()
        else {
            panic!()
        };
        assert_eq!(n1, n2);
        assert_eq!(jname, "New laptop");

        let transfer = sponsor.confirm(&id).unwrap();
        assert!(!transfer.content.contains(&id.secret.to_hex()));
        let JoinerStep::Paired { identity, reply } = joiner.handle(&transfer, 1002).unwrap() else {
            panic!()
        };
        assert_eq!(identity.pubkey(), id.pubkey());
        assert_eq!(identity.secret.to_hex(), id.secret.to_hex());
        assert!(matches!(sponsor.handle(&reply).unwrap(), SponsorStep::Done));
        // Single use.
        assert_eq!(
            joiner.handle(&transfer, 1003).err(),
            Some(PairError::Unexpected)
        );
    }

    #[test]
    fn an_opal_held_identity_pairs_without_its_key() {
        let id = Identity::via_opal(Keys::generate().public_key());
        let mut joiner = Joiner::new("New", 0);
        let (mut sponsor, hello) = Sponsor::start(&joiner.code(), "Desk").unwrap();
        let JoinerStep::ShowNumber { reply, .. } = joiner.handle(&hello, 1).unwrap() else {
            panic!()
        };
        sponsor.handle(&reply).unwrap();
        let transfer = sponsor.confirm(&id).unwrap();
        let JoinerStep::Paired { identity, .. } = joiner.handle(&transfer, 2).unwrap() else {
            panic!()
        };
        assert_eq!(identity.pubkey(), id.pubkey());
        assert!(identity.via_opal_mode());
        assert_eq!(identity.secret.to_hex(), id.secret.to_hex());
    }

    #[test]
    fn someone_who_saw_the_code_changes_the_number() {
        let mut joiner = Joiner::new("New laptop", 0);
        let code = joiner.code();
        // An attacker races in with the code.
        let (mut attacker_to_joiner, evil_hello) = Sponsor::start(&code, "Desk").unwrap();
        let JoinerStep::ShowNumber {
            reply,
            number: joiner_sees,
            ..
        } = joiner.handle(&evil_hello, 1).unwrap()
        else {
            panic!()
        };
        let _ = attacker_to_joiner.handle(&reply).unwrap();
        // ...and poses as the joiner to the real sponsor.
        let (mut real_sponsor, _hello) = Sponsor::start(&code, "Desk").unwrap();
        let fake_joiner = Keys::generate();
        let fake_reply = send(
            &fake_joiner,
            &real_sponsor.me.public_key(),
            &Message::Reply {
                name: "New laptop".into(),
                mac: Code::parse(&code).unwrap().mac(
                    "reply",
                    &[&real_sponsor.me.public_key(), &fake_joiner.public_key()],
                ),
            },
        )
        .unwrap();
        let SponsorStep::ShowNumber {
            number: sponsor_sees,
            ..
        } = real_sponsor.handle(&fake_reply).unwrap()
        else {
            panic!()
        };
        assert_ne!(
            joiner_sees, sponsor_sees,
            "the person comparing screens catches it"
        );
    }

    #[test]
    fn wrong_code_expired_code_and_early_transfer_fail() {
        let mut joiner = Joiner::new("New", 0);
        let (_, hello) = Sponsor::start(&Code::generate().display(), "Desk").unwrap();
        assert_eq!(joiner.handle(&hello, 1).err(), Some(PairError::Invalid));
        let (_, hello) = Sponsor::start(&joiner.code(), "Desk").unwrap();
        assert_eq!(
            joiner.handle(&hello, CODE_LIFETIME + 1).err(),
            Some(PairError::Expired)
        );
        let (mut sponsor, _) = Sponsor::start(&joiner.code(), "Desk").unwrap();
        assert_eq!(
            sponsor.confirm(&Identity::generate()).err(),
            Some(PairError::Unexpected)
        );
        assert_eq!(
            clean_name("\u{1b}[31mEvil\nname  that is much much much longer than forty chars"),
            "[31mEvil name that is much much much lon"
        );
    }
}
