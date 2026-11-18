//! ATT over classic Bluetooth (L2CAP PSM 31): the AirPods keep a few settings
//! there rather than in AACP, among them Loud Sound Reduction. They answer it
//! only for a host with Apple's Device ID (LibrePods' "VendorID spoofing").
//! From LibrePods' linux-rust branch; reads and writes only, one at a time.

use bluer::l2cap::{SeqPacket, Socket, SocketAddr};
use bluer::{Address, AddressType, Error, Result};
use log::{debug, info};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{Duration, timeout};

const PSM_ATT: u16 = 0x001F;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

const OPCODE_ERROR_RESPONSE: u8 = 0x01;
const OPCODE_READ_REQUEST: u8 = 0x0A;
const OPCODE_READ_RESPONSE: u8 = 0x0B;
const OPCODE_WRITE_REQUEST: u8 = 0x12;
const OPCODE_WRITE_RESPONSE: u8 = 0x13;
const OPCODE_HANDLE_VALUE_NTF: u8 = 0x1B;

/// The AirPods' handles (LibrePods' Android app)
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ATTHandles {
    Transparency = 0x18,
    LoudSoundReduction = 0x1B,
    HearingAid = 0x2A,
}

fn io_error(kind: std::io::ErrorKind, what: &str) -> Error {
    Error::from(std::io::Error::new(kind, what.to_string()))
}

#[derive(Default)]
struct Channel {
    sender: Option<mpsc::Sender<Vec<u8>>>,
    tasks: Vec<JoinHandle<()>>,
}

#[derive(Clone)]
pub struct ATTManager {
    channel: Arc<Mutex<Channel>>,
    /// Answers to requests (whole PDUs); notifications don't go here
    responses: Arc<Mutex<mpsc::UnboundedReceiver<Vec<u8>>>>,
    response_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// ATT allows one request at a time: held from sending one to its answer
    request: Arc<Mutex<()>>,
}

impl ATTManager {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        ATTManager {
            channel: Arc::new(Mutex::new(Channel::default())),
            responses: Arc::new(Mutex::new(rx)),
            response_tx: tx,
            request: Arc::new(Mutex::new(())),
        }
    }

    pub async fn connect(&self, addr: Address) -> Result<()> {
        let target = SocketAddr::new(addr, AddressType::BrEdr, PSM_ATT);
        let socket = Socket::new_seq_packet()?;
        let sp = match timeout(CONNECT_TIMEOUT, socket.connect(target)).await {
            Ok(Ok(sp)) => Arc::new(sp),
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => return Err(io_error(std::io::ErrorKind::TimedOut, "ATT connect timed out")),
        };
        info!("{}: ATT channel open", addr);
        let (tx, rx) = mpsc::channel(16);
        let mut channel = self.channel.lock().await;
        channel.sender = Some(tx);
        channel.tasks = vec![
            tokio::spawn(recv_loop(self.clone(), sp.clone())),
            tokio::spawn(send_loop(rx, sp)),
        ];
        Ok(())
    }

    pub async fn is_connected(&self) -> bool {
        self.channel.lock().await.sender.is_some()
    }

    pub async fn shutdown(&self) {
        let mut channel = self.channel.lock().await;
        channel.sender = None;
        for task in channel.tasks.drain(..) {
            task.abort();
        }
    }

    pub async fn read(&self, handle: ATTHandles) -> Result<Vec<u8>> {
        let h = handle as u16;
        let answer = self.request(vec![OPCODE_READ_REQUEST, h as u8, (h >> 8) as u8]).await?;
        match answer.first() {
            Some(&OPCODE_READ_RESPONSE) => Ok(answer[1..].to_vec()),
            _ => Err(unexpected(handle, &answer)),
        }
    }

    pub async fn write(&self, handle: ATTHandles, value: &[u8]) -> Result<()> {
        let h = handle as u16;
        let mut pdu = vec![OPCODE_WRITE_REQUEST, h as u8, (h >> 8) as u8];
        pdu.extend_from_slice(value);
        let answer = self.request(pdu).await?;
        match answer.first() {
            Some(&OPCODE_WRITE_RESPONSE) => Ok(()),
            _ => Err(unexpected(handle, &answer)),
        }
    }

    /// Sends a request and waits for its answer (or the AirPods' error)
    async fn request(&self, pdu: Vec<u8>) -> Result<Vec<u8>> {
        let _one_at_a_time = self.request.lock().await;
        let mut responses = self.responses.lock().await;
        // An answer that came after its request timed out isn't this one's
        while responses.try_recv().is_ok() {}
        let sender = self.channel.lock().await.sender.clone();
        let sender = sender.ok_or_else(|| io_error(std::io::ErrorKind::NotConnected, "ATT channel not open"))?;
        sender
            .send(pdu)
            .await
            .map_err(|_| io_error(std::io::ErrorKind::NotConnected, "ATT channel closed"))?;
        match timeout(RESPONSE_TIMEOUT, responses.recv()).await {
            Ok(Some(answer)) => Ok(answer),
            Ok(None) => Err(io_error(std::io::ErrorKind::UnexpectedEof, "ATT channel closed")),
            Err(_) => Err(io_error(std::io::ErrorKind::TimedOut, "no ATT answer")),
        }
    }
}

/// An error response (`01 <request> <handle> <code>`), or anything else
fn unexpected(handle: ATTHandles, answer: &[u8]) -> Error {
    let what = if answer.first() == Some(&OPCODE_ERROR_RESPONSE) && answer.len() >= 5 {
        format!("ATT error {:#04x} for {:?}", answer[4], handle)
    } else {
        format!("unexpected ATT answer for {:?}: {}", handle, hex::encode(answer))
    };
    io_error(std::io::ErrorKind::Other, &what)
}

async fn recv_loop(manager: ATTManager, sp: Arc<SeqPacket>) {
    let mut buf = vec![0u8; 1024];
    loop {
        match sp.recv(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let pdu = &buf[..n];
                debug!("ATT received: {}", hex::encode(pdu));
                // Notifications aren't asked for (LibrePods: they don't come
                // for Loud Sound Reduction); the rest answers a request
                if pdu.first() != Some(&OPCODE_HANDLE_VALUE_NTF) {
                    let _ = manager.response_tx.send(pdu.to_vec());
                }
            }
            Err(e) => {
                debug!("ATT read error: {}", e);
                break;
            }
        }
    }
    info!("ATT channel closed");
    manager.channel.lock().await.sender = None;
}

async fn send_loop(mut rx: mpsc::Receiver<Vec<u8>>, sp: Arc<SeqPacket>) {
    while let Some(pdu) = rx.recv().await {
        if let Err(e) = sp.send(&pdu).await {
            debug!("ATT send failed: {}", e);
            break;
        }
        debug!("ATT sent: {}", hex::encode(&pdu));
    }
}
