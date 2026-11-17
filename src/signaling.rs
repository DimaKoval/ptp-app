//! Serverless signaling over public Nostr relays.
//!
//! The room link carries a random 32-byte secret in its URL fragment (never sent
//! to any server). From it both sides derive:
//!   - `topic` – a public tag used to find each other on the relays
//!   - `key`   – AES-256-GCM key; every signaling payload is encrypted with it
//!
//! Messages are ephemeral Nostr events (kind 25050), so relays forward them but
//! do not store them. The relays only ever see opaque ciphertext.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use secp256k1::Keypair;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

pub const EVENT_KIND: u32 = 25050;

pub const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.primal.net",
    "wss://nostr.mom",
];

// ───────────────────────────── room ─────────────────────────────

#[derive(Clone)]
pub struct Room {
    pub secret: [u8; 32],
    pub topic: String,
    key: [u8; 32],
}

impl Room {
    pub fn generate() -> Room {
        Room::from_secret(rand::random())
    }

    pub fn from_secret(secret: [u8; 32]) -> Room {
        let topic = hex(&sha256_parts(&[b"ptp-share/topic/v1", &secret]));
        let key = sha256_parts(&[b"ptp-share/key/v1", &secret]);
        Room { secret, topic, key }
    }

    /// Viewer link: `<base>#k=<secret>[&r=<relays>]`.
    pub fn link(&self, base_url: &str, relays: &[String]) -> String {
        let mut url = format!("{}#k={}", base_url.trim_end_matches('#'), URL_SAFE_NO_PAD.encode(self.secret));
        let defaults: Vec<String> = DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect();
        if relays != defaults.as_slice() && !relays.is_empty() {
            url.push_str("&r=");
            url.push_str(&relays.iter().map(|r| urlencode(r)).collect::<Vec<_>>().join(","));
        }
        url
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> String {
        let cipher = Aes256Gcm::new_from_slice(&self.key).expect("key length");
        let nonce_bytes: [u8; 12] = rand::random();
        let ct = cipher
            .encrypt(&Nonce::from(nonce_bytes), plaintext)
            .expect("encryption cannot fail");
        let mut out = nonce_bytes.to_vec();
        out.extend_from_slice(&ct);
        STANDARD.encode(out)
    }

    pub fn decrypt(&self, content: &str) -> Result<Vec<u8>> {
        let raw = STANDARD.decode(content.trim())?;
        if raw.len() < 12 + 16 {
            return Err(anyhow!("ciphertext too short"));
        }
        let cipher = Aes256Gcm::new_from_slice(&self.key).expect("key length");
        let nonce: [u8; 12] = raw[..12].try_into()?;
        cipher
            .decrypt(&Nonce::from(nonce), &raw[12..])
            .map_err(|_| anyhow!("decryption failed"))
    }
}

fn sha256_parts(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// ───────────────────────────── protocol ─────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub username: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub credential: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalMessage {
    /// viewer → host: "I want to watch"
    Hello {
        sid: String,
        #[serde(default)]
        label: String,
    },
    /// host → viewer
    Offer {
        sid: String,
        sdp: String,
        ice_servers: Vec<IceServer>,
        width: u32,
        height: u32,
        fps: u32,
    },
    /// viewer → host
    Answer { sid: String, sdp: String },
    /// either way: session closed
    Bye { sid: String },
    /// host → viewer: stream is over
    Ended,
}

/// A decrypted message plus who sent it.
#[derive(Debug, Clone)]
pub struct Incoming {
    pub from_pubkey: String,
    pub msg: SignalMessage,
}

// ───────────────────────────── nostr events ─────────────────────────────

pub struct Identity {
    keypair: Keypair,
    pub pubkey: String,
}

impl Identity {
    pub fn generate() -> Identity {
        loop {
            let bytes: [u8; 32] = rand::random();
            if let Ok(kp) = Keypair::from_secret_bytes(bytes) {
                let pubkey = hex(&kp.x_only_public_key().0.to_byte_array());
                return Identity { keypair: kp, pubkey };
            }
        }
    }

    pub fn sign_event(&self, kind: u32, tags: Vec<Vec<String>>, content: String) -> Value {
        let created_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let serialized = serde_json::to_string(&json!([0, self.pubkey, created_at, kind, tags, content])).unwrap();
        let id = sha256_parts(&[serialized.as_bytes()]);
        let sig = self.keypair.sign_schnorr_no_aux_rand(&id);
        json!({
            "id": hex(&id),
            "pubkey": self.pubkey,
            "created_at": created_at,
            "kind": kind,
            "tags": tags,
            "content": content,
            "sig": hex(&sig.to_byte_array()),
        })
    }
}

/// How far (seconds) a peer's clock may differ from ours before relays start
/// filtering out its messages. Relays apply `since` to live events too.
const CLOCK_SKEW_SECS: u64 = 60;

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

// ───────────────────────────── relay pool ─────────────────────────────

#[derive(Debug, Clone)]
pub struct RelayStatus {
    pub url: String,
    pub connected: bool,
    pub last_error: Option<String>,
}

/// Connection to a set of relays: publishes our events to all of them and
/// delivers decrypted messages addressed to our room.
pub struct Signaling {
    pub room: Room,
    pub identity: Identity,
    out_tx: broadcast::Sender<String>,
    pub status: Arc<Mutex<Vec<RelayStatus>>>,
}

impl Signaling {
    /// Connects to `relays` and subscribes to the room topic. Incoming messages
    /// go to the returned receiver. Stops when `shutdown` flips to true.
    pub fn start(
        room: Room,
        relays: Vec<String>,
        shutdown: watch::Receiver<bool>,
    ) -> (Arc<Signaling>, mpsc::UnboundedReceiver<Incoming>) {
        let identity = Identity::generate();
        let (out_tx, _) = broadcast::channel::<String>(256);
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let status = Arc::new(Mutex::new(
            relays
                .iter()
                .map(|u| RelayStatus { url: u.clone(), connected: false, last_error: None })
                .collect::<Vec<_>>(),
        ));
        let sig = Arc::new(Signaling { room, identity, out_tx, status });
        let seen = Arc::new(Mutex::new(SeenSet::default()));
        for (i, url) in relays.into_iter().enumerate() {
            tokio::spawn(relay_task(i, url, sig.clone(), in_tx.clone(), seen.clone(), shutdown.clone()));
        }
        (sig, in_rx)
    }

    pub fn send(&self, to_pubkey: Option<&str>, msg: &SignalMessage) {
        let plaintext = serde_json::to_vec(msg).unwrap();
        let content = self.room.encrypt(&plaintext);
        let mut tags = vec![vec!["t".to_string(), self.room.topic.clone()]];
        if let Some(p) = to_pubkey {
            tags.push(vec!["p".to_string(), p.to_string()]);
        }
        let event = self.identity.sign_event(EVENT_KIND, tags, content);
        let frame = json!(["EVENT", event]).to_string();
        let _ = self.out_tx.send(frame);
    }

}

#[derive(Default)]
struct SeenSet {
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl SeenSet {
    fn insert(&mut self, id: &str) -> bool {
        if self.set.contains(id) {
            return false;
        }
        self.set.insert(id.to_string());
        self.order.push_back(id.to_string());
        if self.order.len() > 2048 {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }
}

async fn relay_task(
    index: usize,
    url: String,
    sig: Arc<Signaling>,
    in_tx: mpsc::UnboundedSender<Incoming>,
    seen: Arc<Mutex<SeenSet>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if *shutdown.borrow() {
            return;
        }
        let result = tokio::select! {
            r = relay_session(index, &url, &sig, &in_tx, &seen) => r,
            _ = shutdown.changed() => return,
        };
        {
            let mut st = sig.status.lock().unwrap();
            st[index].connected = false;
            if let Err(e) = &result {
                st[index].last_error = Some(e.to_string());
            }
        }
        if let Err(e) = result {
            eprintln!("[relay {url}] {e}");
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown.changed() => return,
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn relay_session(
    index: usize,
    url: &str,
    sig: &Arc<Signaling>,
    in_tx: &mpsc::UnboundedSender<Incoming>,
    seen: &Arc<Mutex<SeenSet>>,
) -> Result<()> {
    let (ws, _) = tokio::time::timeout(Duration::from_secs(8), tokio_tungstenite::connect_async(url))
        .await
        .map_err(|_| anyhow!("connect timeout"))??;
    let (mut write, mut read) = ws.split();
    // Subscribe before marking connected so early events aren't lost.
    let mut out_rx = sig.out_tx.subscribe();
    let filter = json!({
        "kinds": [EVENT_KIND],
        "#t": [sig.room.topic],
        "since": now_secs().saturating_sub(CLOCK_SKEW_SECS),
    });
    write.send(Message::text(json!(["REQ", "ptp", filter]).to_string())).await?;
    {
        let mut st = sig.status.lock().unwrap();
        st[index].connected = true;
        st[index].last_error = None;
    }
    let mut ping = tokio::time::interval(Duration::from_secs(25));
    ping.tick().await;
    loop {
        tokio::select! {
            frame = out_rx.recv() => {
                match frame {
                    Ok(f) => write.send(Message::text(f)).await?,
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
            msg = read.next() => {
                let msg = match msg {
                    Some(m) => m?,
                    None => return Err(anyhow!("connection closed")),
                };
                match msg {
                    Message::Text(t) => handle_relay_frame(url, t.as_str(), sig, in_tx, seen),
                    Message::Ping(p) => write.send(Message::Pong(p)).await?,
                    Message::Close(_) => return Err(anyhow!("closed by relay")),
                    _ => {}
                }
            }
            _ = ping.tick() => {
                write.send(Message::Ping(Vec::new().into())).await?;
            }
        }
    }
}

fn handle_relay_frame(
    url: &str,
    text: &str,
    sig: &Arc<Signaling>,
    in_tx: &mpsc::UnboundedSender<Incoming>,
    seen: &Arc<Mutex<SeenSet>>,
) {
    let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(text) else { return };
    match arr.first().and_then(|v| v.as_str()) {
        Some("EVENT") => {
            let Some(ev) = arr.get(2) else { return };
            let id = ev["id"].as_str().unwrap_or_default();
            let pubkey = ev["pubkey"].as_str().unwrap_or_default();
            if pubkey == sig.identity.pubkey || !seen.lock().unwrap().insert(id) {
                return;
            }
            let Some(content) = ev["content"].as_str() else { return };
            let Ok(plain) = sig.room.decrypt(content) else { return };
            match serde_json::from_slice::<SignalMessage>(&plain) {
                Ok(msg) => {
                    let _ = in_tx.send(Incoming { from_pubkey: pubkey.to_string(), msg });
                }
                Err(e) => eprintln!("[relay {url}] bad message: {e}"),
            }
        }
        Some("OK") => {
            if arr.get(2).and_then(|v| v.as_bool()) == Some(false) {
                eprintln!("[relay {url}] rejected event: {}", arr.get(3).cloned().unwrap_or_default());
            }
        }
        Some("NOTICE") | Some("CLOSED") => eprintln!("[relay {url}] {text}"),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_encryption() {
        let room = Room::generate();
        let ct = room.encrypt(b"hello");
        assert_eq!(room.decrypt(&ct).unwrap(), b"hello");
        let other = Room::generate();
        assert!(other.decrypt(&ct).is_err());
    }

    #[test]
    fn signature_verifies() {
        let id = Identity::generate();
        let ev = id.sign_event(EVENT_KIND, vec![vec!["t".into(), "x".into()]], "c".into());
        let idb: Vec<u8> = (0..32)
            .map(|i| u8::from_str_radix(&ev["id"].as_str().unwrap()[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        let sigb: [u8; 64] = (0..64)
            .map(|i| u8::from_str_radix(&ev["sig"].as_str().unwrap()[i * 2..i * 2 + 2], 16).unwrap())
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        let sig = secp256k1::schnorr::Signature::from_byte_array(sigb);
        let pk = id.keypair.x_only_public_key().0;
        assert!(secp256k1::schnorr::verify(&sig, &idb, &pk).is_ok());
    }
}
