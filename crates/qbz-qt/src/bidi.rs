//! A small WebDriver BiDi client: the remote protocol Firefox speaks over a local
//! WebSocket (`--remote-debugging-port`). `firefox_engine.rs` uses it to run Qobuz's
//! official player in Firefox. Only what that needs: connect, send a command, wait
//! for its answer. Plain tokio TCP and RFC 6455 framing, so no new dependency.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

pub struct Bidi {
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
    pending: Pending,
    next_id: AtomicU64,
    closed: Arc<AtomicBool>,
}

/// Client-side frames must be masked; the mask only has to be unpredictable to
/// proxies, which a local socket does not have.
fn noise(seed: &mut u64) -> u32 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed as u32
}

fn seed() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1) | 1
}

impl Bidi {
    /// Open the BiDi session endpoint of a Firefox started with
    /// `--remote-debugging-port <port>`.
    pub async fn connect(port: u16) -> Result<Arc<Bidi>, String> {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| e.to_string())?;
        let _ = stream.set_nodelay(true);
        let (mut rd, mut wr) = stream.into_split();
        let mut s = seed();
        let key_bytes: Vec<u8> = (0..4).flat_map(|_| noise(&mut s).to_le_bytes()).collect();
        let key = base64::engine::general_purpose::STANDARD.encode(key_bytes);
        let request = format!(
            "GET /session HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        wr.write_all(request.as_bytes()).await.map_err(|e| e.to_string())?;
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            rd.read_exact(&mut byte).await.map_err(|e| e.to_string())?;
            head.push(byte[0]);
            if head.len() > 16 * 1024 {
                return Err("oversized handshake answer".into());
            }
        }
        let status = String::from_utf8_lossy(&head);
        if !status.starts_with("HTTP/1.1 101") {
            return Err(format!("handshake refused: {}", status.lines().next().unwrap_or("")));
        }
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let bidi = Arc::new(Bidi {
            writer: tokio::sync::Mutex::new(wr),
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
            closed: closed.clone(),
        });
        tokio::spawn(read_loop(rd, pending, closed, Arc::downgrade(&bidi)));
        Ok(bidi)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Send one command and wait for its result (or its error).
    pub async fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        if self.is_closed() {
            return Err("connection closed".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let message = json!({ "id": id, "method": method, "params": params }).to_string();
        if let Err(e) = self.send_frame(0x1, message.as_bytes()).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(answer)) => {
                if answer.get("type").and_then(Value::as_str) == Some("error") {
                    let error = answer.get("error").and_then(Value::as_str).unwrap_or("error");
                    let text = answer.get("message").and_then(Value::as_str).unwrap_or("");
                    return Err(format!("{method}: {error}: {}", text.chars().take(200).collect::<String>()));
                }
                Ok(answer.get("result").cloned().unwrap_or(Value::Null))
            }
            Ok(Err(_)) => Err("connection closed".into()),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!("{method}: no answer"))
            }
        }
    }

    /// Run `expression` in the page of `context` and return its value (strings,
    /// numbers and booleans; `Null` for undefined). A thrown exception is an error.
    pub async fn evaluate(&self, context: &str, expression: &str) -> Result<Value, String> {
        let result = self
            .call(
                "script.evaluate",
                json!({
                    "expression": expression,
                    "target": { "context": context },
                    "awaitPromise": false,
                    "resultOwnership": "none",
                }),
                Duration::from_secs(20),
            )
            .await?;
        if result.get("type").and_then(Value::as_str) == Some("exception") {
            let text = result
                .pointer("/exceptionDetails/text")
                .and_then(Value::as_str)
                .unwrap_or("exception");
            return Err(format!("page exception: {}", text.chars().take(200).collect::<String>()));
        }
        Ok(result.pointer("/result/value").cloned().unwrap_or(Value::Null))
    }

    async fn send_frame(&self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        let len = payload.len();
        if len < 126 {
            frame.push(0x80 | len as u8);
        } else if len <= 0xFFFF {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
        let mut s = seed();
        let mask = noise(&mut s).to_le_bytes();
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        let mut writer = self.writer.lock().await;
        writer.write_all(&frame).await.map_err(|e| e.to_string())
    }
}

async fn read_loop(mut rd: OwnedReadHalf, pending: Pending, closed: Arc<AtomicBool>, bidi: Weak<Bidi>) {
    let mut message: Vec<u8> = Vec::new();
    loop {
        let mut head = [0u8; 2];
        if rd.read_exact(&mut head).await.is_err() {
            break;
        }
        let fin = head[0] & 0x80 != 0;
        let opcode = head[0] & 0x0F;
        let masked = head[1] & 0x80 != 0;
        let mut len = u64::from(head[1] & 0x7F);
        if len == 126 {
            let mut b = [0u8; 2];
            if rd.read_exact(&mut b).await.is_err() {
                break;
            }
            len = u64::from(u16::from_be_bytes(b));
        } else if len == 127 {
            let mut b = [0u8; 8];
            if rd.read_exact(&mut b).await.is_err() {
                break;
            }
            len = u64::from_be_bytes(b);
        }
        let mut mask = [0u8; 4];
        if masked && rd.read_exact(&mut mask).await.is_err() {
            break;
        }
        if len > 256 * 1024 * 1024 {
            break;
        }
        let mut payload = vec![0u8; len as usize];
        if rd.read_exact(&mut payload).await.is_err() {
            break;
        }
        if masked {
            payload.iter_mut().enumerate().for_each(|(i, b)| *b ^= mask[i % 4]);
        }
        match opcode {
            0x0 | 0x1 | 0x2 => {
                message.extend_from_slice(&payload);
                if fin {
                    if let Ok(value) = serde_json::from_slice::<Value>(&message) {
                        if let Some(id) = value.get("id").and_then(Value::as_u64) {
                            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                                let _ = tx.send(value);
                            }
                        }
                    }
                    message.clear();
                }
            }
            0x8 => break,
            0x9 => {
                if let Some(b) = bidi.upgrade() {
                    let _ = b.send_frame(0xA, &payload).await;
                }
            }
            _ => {}
        }
    }
    closed.store(true, Ordering::SeqCst);
    pending.lock().unwrap().clear();
}
