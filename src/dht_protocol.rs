use std::net::SocketAddr;
use tokio::net::UdpSocket;
use thiserror::Error;
use bendy::encoding::{ToBencode, SingleItemEncoder, Encoder};
use bendy::value::Value;
use crate::torrent::TorrentInfo;

/// DHT (Distributed Hash Table) protocol implementation for peer discovery and communication.
const DEFAULT_NETWORKS: &[&str] = &["router.bittorrent.com:6881", "dht.transmissionbt.com:6881", "router.utorrent.com:6881"];

pub async fn dht_search(torrent: TorrentInfo, peer_id: String) -> Vec<SocketAddr> {
    println!("DHT search started for torrent: {}", torrent.name);
    println!("Peer ID: {}", peer_id);

    let all_peers: Vec<SocketAddr> = Vec::new();
    let networks = if !torrent.nodes.is_empty() {
        torrent.nodes.iter().map(|(host, port)| format!("{}:{}", host, port)).collect::<Vec<_>>()
    } else {
        DEFAULT_NETWORKS.iter().map(|&s| s.to_string()).collect::<Vec<_>>()
    };

    for network in networks {
        println!("Contacting DHT node: {}", network);
        let addr: SocketAddr = UdpSocket::bind("0.0.0.0:0").await.unwrap().local_addr().unwrap();

    }

    println!("DHT search discovered {} peers.", all_peers.len());

    return all_peers;
}

/// Build a DHT ping message.


pub async fn build_ping_message() -> Vec<u8> {

    todo!()
}