use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};
use uuid::Uuid;

use crate::config::AppConfig;
use crate::network::AppEvent;

const DISCOVERY_MAGIC: &str = "FASTTRAN_DISCOVERY";
const DISCOVERY_VERSION: u16 = 1;
const PEER_TIMEOUT: Duration = Duration::from_secs(8);
const BROADCAST_INTERVAL: Duration = Duration::from_secs(2);
const MAX_DATAGRAM_SIZE: usize = 4 * 1024;

#[derive(Debug, Clone)]
pub struct Peer {
    pub id: Uuid,
    pub name: String,
    pub address: Ipv4Addr,
    pub transfer_port: u16,
    pub os: String,
    pub last_seen: Instant,
}

impl Peer {
    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(self.address), self.transfer_port)
    }

    pub fn display_address(&self) -> String {
        format!("{}:{}", self.address, self.transfer_port)
    }

    pub fn manual(address: Ipv4Addr, transfer_port: u16) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: format!("手动设备 {address}"),
            address,
            transfer_port,
            os: "unknown".to_owned(),
            last_seen: Instant::now(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Advertisement {
    magic: String,
    version: u16,
    id: Uuid,
    name: String,
    transfer_port: u16,
    os: String,
}

pub struct DiscoveryService {
    local_port: u16,
    peers: Arc<RwLock<Vec<Peer>>>,
    shutdown: Arc<Notify>,
    stopping: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl DiscoveryService {
    pub async fn start(
        config: Arc<RwLock<AppConfig>>,
        events: tokio::sync::mpsc::UnboundedSender<AppEvent>,
    ) -> Result<Self> {
        let preferred_port = config
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .discovery_port;
        let (socket, local_port) = bind_udp_socket(preferred_port)
            .await
            .context("无法启动局域网设备发现")?;
        socket.set_broadcast(true).context("无法启用 UDP 广播")?;

        let peers = Arc::new(RwLock::new(Vec::new()));
        let shutdown = Arc::new(Notify::new());
        let stopping = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(run_discovery(
            socket,
            local_port,
            config,
            peers.clone(),
            shutdown.clone(),
            stopping.clone(),
            events,
        ));

        Ok(Self {
            local_port,
            peers,
            shutdown,
            stopping,
            task,
        })
    }

    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    pub fn peers(&self) -> Vec<Peer> {
        let mut peers = self
            .peers
            .write()
            .unwrap_or_else(|error| error.into_inner());
        peers.retain(|peer| peer.last_seen.elapsed() <= PEER_TIMEOUT);
        peers.sort_by_key(|peer| peer.name.to_lowercase());
        peers.clone()
    }
}

impl Drop for DiscoveryService {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.shutdown.notify_waiters();
        self.task.abort();
    }
}

async fn bind_udp_socket(preferred_port: u16) -> Result<(UdpSocket, u16)> {
    let mut last_error = None;
    for offset in 0..32_u16 {
        let Some(port) = preferred_port.checked_add(offset) else {
            break;
        };
        match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).await {
            Ok(socket) => return Ok((socket, port)),
            Err(error) if error.kind() == ErrorKind::AddrInUse => {
                last_error = Some(error);
            }
            Err(error) => return Err(error.into()),
        }
    }

    Err(last_error
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("no usable discovery port")))
}

async fn run_discovery(
    socket: UdpSocket,
    local_port: u16,
    config: Arc<RwLock<AppConfig>>,
    peers: Arc<RwLock<Vec<Peer>>>,
    shutdown: Arc<Notify>,
    stopping: Arc<AtomicBool>,
    events: tokio::sync::mpsc::UnboundedSender<AppEvent>,
) {
    let mut ticker = interval(BROADCAST_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let broadcast_addresses = broadcast_addresses();
    let mut buffer = vec![0_u8; MAX_DATAGRAM_SIZE];

    loop {
        if stopping.load(Ordering::Acquire) {
            return;
        }
        tokio::select! {
            _ = shutdown.notified() => return,
            _ = ticker.tick() => {
                if stopping.load(Ordering::Acquire) {
                    return;
                }
                let advertisement = {
                    let config = config.read().unwrap_or_else(|error| error.into_inner());
                    Advertisement {
                        magic: DISCOVERY_MAGIC.to_owned(),
                        version: DISCOVERY_VERSION,
                        id: config.device_id,
                        name: config.device_name.clone(),
                        transfer_port: config.transfer_port,
                        os: std::env::consts::OS.to_owned(),
                    }
                };
                let payload = match serde_json::to_vec(&advertisement) {
                    Ok(payload) => payload,
                    Err(error) => {
                        let _ = events.send(AppEvent::Error {
                            scope: "设备发现",
                            message: error.to_string(),
                        });
                        continue;
                    }
                };

                for address in &broadcast_addresses {
                    if stopping.load(Ordering::Acquire) {
                        return;
                    }
                    let destination = SocketAddr::new(IpAddr::V4(*address), local_port);
                    if let Err(error) = socket.send_to(&payload, destination).await {
                        tracing::debug!(%error, %destination, "device discovery broadcast failed");
                    }
                }
            }
            received = socket.recv_from(&mut buffer) => {
                if stopping.load(Ordering::Acquire) {
                    return;
                }
                match received {
                    Ok((length, source)) => {
                        let Ok(advertisement) = serde_json::from_slice::<Advertisement>(&buffer[..length]) else {
                            continue;
                        };
                        if advertisement.magic != DISCOVERY_MAGIC
                            || advertisement.version != DISCOVERY_VERSION
                            || advertisement.transfer_port == 0
                        {
                            continue;
                        }

                        let self_id = config
                            .read()
                            .unwrap_or_else(|error| error.into_inner())
                            .device_id;
                        if advertisement.id == self_id {
                            continue;
                        }
                        let IpAddr::V4(address) = source.ip() else {
                            continue;
                        };
                        let name: String = advertisement
                            .name
                            .chars()
                            .filter(|character| !character.is_control())
                            .take(32)
                            .collect();
                        let name = if name.trim().is_empty() {
                            "FastTran Device".to_owned()
                        } else {
                            name.trim().to_owned()
                        };
                        let os: String = advertisement
                            .os
                            .chars()
                            .filter(|character| !character.is_control())
                            .take(16)
                            .collect();
                        let os = if os.trim().is_empty() {
                            "unknown".to_owned()
                        } else {
                            os.trim().to_owned()
                        };
                        let peer = Peer {
                            id: advertisement.id,
                            name,
                            address,
                            transfer_port: advertisement.transfer_port,
                            os,
                            last_seen: Instant::now(),
                        };

                        let mut peers = peers.write().unwrap_or_else(|error| error.into_inner());
                        peers.retain(|existing| existing.id != peer.id);
                        peers.push(peer);
                    }
                    Err(error) => {
                        tracing::debug!(%error, "device discovery receive failed");
                    }
                }
            }
        }
    }
}

fn broadcast_addresses() -> Vec<Ipv4Addr> {
    let mut addresses = Vec::new();
    for interface in if_addrs::get_if_addrs().unwrap_or_default() {
        if interface.is_loopback() {
            continue;
        }
        if let if_addrs::IfAddr::V4(info) = &interface.addr {
            if let Some(broadcast) = info.broadcast {
                if !broadcast.is_unspecified() && !addresses.contains(&broadcast) {
                    addresses.push(broadcast);
                }
            } else if !info.ip.is_unspecified() && !info.ip.is_loopback() {
                // Some virtual adapters omit a broadcast address. Derive one
                // from the interface netmask as a best-effort fallback.
                let ip = info.ip.octets();
                let mask = info.netmask.octets();
                let fallback = Ipv4Addr::new(
                    ip[0] | !mask[0],
                    ip[1] | !mask[1],
                    ip[2] | !mask[2],
                    ip[3] | !mask[3],
                );
                if !fallback.is_unspecified() && !addresses.contains(&fallback) {
                    addresses.push(fallback);
                }
            }
        }
    }

    let global = Ipv4Addr::new(255, 255, 255, 255);
    if !addresses.contains(&global) {
        addresses.push(global);
    }
    addresses
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::Peer;

    #[test]
    fn manual_peer_formats_socket_address() {
        let peer = Peer::manual(Ipv4Addr::new(192, 168, 1, 8), 45_455);
        assert_eq!(peer.socket_addr().to_string(), "192.168.1.8:45455");
        assert_eq!(peer.display_address(), "192.168.1.8:45455");
    }
}
