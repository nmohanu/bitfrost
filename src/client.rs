use tokio::{io::AsyncWriteExt, io::AsyncReadExt, net::TcpStream};

use thiserror::Error;
use reqwest::Client;

use rand::RngCore;
use hex::ToHex;

use bendy::value::Value;
use bendy::decoding::FromBencode;

use urlencoding;

use std::net::{SocketAddr, Ipv4Addr};

use crate::torrent::{TorrentInfo, get_torrent_info_hash};

const DEFAULT_PORT: u16 = 6881;
const CLIENT_ID : &str = "-BF0001-"; // BitFrost client identifier

pub struct TorrentClient {
    _torrent: TorrentInfo,
    _peer_id : [u8; 20],
}

impl TorrentClient {
    pub fn new(torrent: TorrentInfo) -> Self {
        Self { _torrent: torrent, _peer_id: [0; 20] }
    }

    pub async fn start(&mut self) -> Result<(), Error> {
        self._peer_id = get_client_id(); 
        println!("Starting torrent client for: {}", self._torrent.name);
        println!("Info Hash: {:x?}", self._torrent.info_hash);
        println!("Getting peers from tracker...");
        let peers = fetch_peers(&self._torrent, self._peer_id).await?;
        println!("Found {} peers", peers.len());
        for peer in peers {
            println!("{}", peer.to_string());
            perform_handshake(peer, self._torrent.info_hash, self._peer_id).await?;
        }
        Ok(())
    }
}

pub fn create_handshake(info_hash: [u8; 20], peer_id: [u8; 20]) -> [u8; 68] {
    let mut handshake = [0u8; 68];
    handshake[0] = 19; 
    handshake[1..20].copy_from_slice(b"BitTorrent protocol"); 
    handshake[28..48].copy_from_slice(&info_hash); // info_hash.
    handshake[48..68].copy_from_slice(&peer_id); // peer_id.
    handshake
}

pub async fn perform_handshake(peer: SocketAddr, info_hash: [u8; 20], peer_id: [u8; 20]) -> Result<TcpStream, Error> {
    let mut stream = TcpStream::connect(peer.to_string()).await?;
    // Perform the BitTorrent handshake.
    let handshake = create_handshake(info_hash, peer_id);
    println!("Sending handshake to {}: {:x?}", peer, &handshake);
    stream.write_all(&handshake).await?;

    // Get response from peer.
    let mut response = [0u8; 68];
    stream.read_exact(&mut response).await?;

    println!("Received handshake response from {}: {:x?}", peer, (&response[48..68]).encode_hex::<String>());

    Ok(stream)
}

pub fn get_client_id() -> [u8; 20] {
    let mut peer_id = [0u8; 20];
    peer_id[..8].copy_from_slice(CLIENT_ID.as_bytes());
    rand::rng().fill_bytes(&mut peer_id[8..]);
    peer_id
}

pub async fn fetch_peers(torrent_info: &TorrentInfo, id: [u8; 20]) -> Result<Vec<SocketAddr>, Error> {
    let client = Client::new();

    let url = format!(
        "{}?info_hash={}&peer_id={}&port={}&uploaded=0&downloaded=0&left={}&compact=1",
        torrent_info.announce,
        urlencoding::encode_binary(&torrent_info.info_hash),
        urlencoding::encode_binary(&id),
        DEFAULT_PORT,
        torrent_info.length.unwrap_or(0)
    );

    println!("Sending out: {}", url);

    // Send the GET request to the tracker.
    let response = client.get(&url).send().await?.bytes().await?;
    let val = Value::from_bencode(&response)
        .map_err(|e| Error::StartError(format!("Bencode decoding error: {}", e)))?;
    
    let dict = match val {
        Value::Dict(d) => d,
        _ => return Err(Error::StartError("Expected dictionary response".into())),
    };

    let peers_bytes = match dict.get(&b"peers"[..]) {
        Some(Value::Bytes(b)) => b,
        _ => return Err(Error::StartError("Missing 'peers' field".into())),
    };

    // Parse the compact peer list.
    let mut peers = Vec::new();
    for chunk in peers_bytes.chunks_exact(6) {
        let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
        let port = u16::from_be_bytes([chunk[4], chunk[5]]);
        peers.push(SocketAddr::new(ip.into(), port));
    }

    Ok(peers)
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("Error with starting the client: {0}")]
    StartError(String),

    #[error(transparent)]
    Connection(#[from] reqwest::Error),

    #[error(transparent)]
    Handshake(#[from] std::io::Error),
}