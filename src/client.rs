use sha1::{Digest, Sha1};

use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::{TcpStream, UdpSocket}};
use tokio::time::{timeout, Duration};

use thiserror::Error;
use reqwest::Client;

use rand::{RngCore};
use hex::ToHex;

use bip_bencode::{BDecodeOpt, BRefAccess, BencodeRef};

use urlencoding;

use std::{collections::HashMap, net::{Ipv4Addr, SocketAddr}, usize};

use crate::{torrent::TorrentInfo, util::vec_u8_to_box_bool, bitfield_actor::get_requestable_piece};
use crate::bitfield_actor::{BitfieldMsg, bitfield_actor, BitfieldSender};
use crate::output_actor::{OutputMsg, output_actor, OutputSender};
use crate::dht_protocol::dht_search;

const DEFAULT_PORT: u16 = 6881; 
const CLIENT_ID : &str = "-BF0001-"; // BitFrost client identifier.
const BLOCK_SIZE : usize = 16384; // 16KB block size.
const MAX_WORKER_REQUESTS: usize = 5; // Max pending requests per worker.
const MAX_CONCURRENT_PEERS: usize = 30; // Limit concurrent peer connections.

/// A worker that manages communication with a single peer.
/// It handles the BitTorrent protocol messages, piece requests, and data verification.
/// It communicates with the bitfield actor to update piece availability.
/// After downloading a piece, it verifies the piece hash and updates the bitfield accordingly.
/// if the piece verification fails, it notifies the bitfield actor to mark the piece as failed.
/// This allows the client to retry failed pieces later.
pub struct PeerWorker {
    stream: TcpStream,
    // Pending requests mapped to whether they've been fulfilled.
    pending_requests: HashMap<u32, bool>,
    torrent: TorrentInfo,
    bitfield_tx: BitfieldSender,
    output_tx: OutputSender,
    peer_addr: SocketAddr,
    peer_bitfield: Option<Box<[bool]>>,
}

/// The state for the download client. 
pub struct TorrentClient {
    torrent: TorrentInfo,
    output_path: String,
}

impl TorrentClient {
    /// Create a new TorrentClient. The output path is derived from the torrent name.
    /// TODO: Allow specifying custom output path.
    pub fn new(torrent: TorrentInfo) -> Self {
        let torrent_name = torrent.name.clone();
        Self {
            torrent,
            output_path: format!("./{}", torrent_name),
        }
    }

    /// Start the torrent client. This sets up the bitfield actor, file output, and starts the
    /// download process.
    pub async fn start(&mut self) -> Result<(), Error> {
        // Spawn the bitfield actor.
        let (bitfield_tx, bitfield_rx) = tokio::sync::mpsc::channel(32);
        let (output_tx, output_rx) = tokio::sync::mpsc::channel(32);
        // TODO: load existing bitfield from disk if resuming.
        let initial_bitfield: Box<[bool]> = vec_u8_to_box_bool(vec![0; (self.torrent.pieces.len() / 20 + 7) / 8]);

        // Spawn the bitfield actor. This is a long-lived task that manages the bitfield state.
        // the actor will receive messages from workers via tx.
        tokio::spawn(bitfield_actor(bitfield_rx, initial_bitfield));
        
        // Spawn the output actor. This handles writing pieces to disk.
        tokio::spawn(output_actor(output_rx, self.torrent.clone()));

        // Set up the file destination.
        // Remove existing file if it exists.
        // TODO: pick up where we left off instead of deleting.
        if tokio::fs::metadata(&self.output_path).await.is_ok() {
            tokio::fs::remove_file(&self.output_path).await.expect("Failed to delete file");
        }

        println!("Starting torrent client for: {}", self.torrent.name);
        self.download_file(bitfield_tx, output_tx).await?;
        Ok(())
    }

    /// Download the entire file.
    /// Here we manage peer connections and spawn workers.
    async fn download_file(&self, bitfield_tx: BitfieldSender, output_tx: OutputSender) -> Result<(), Error> {
        use futures::stream::{FuturesUnordered, StreamExt};
        use tokio::sync::oneshot;
        let peer_id = get_client_id();
        let info_hash = self.torrent.info_hash;

        loop {
            // Check if download is complete by asking the bitfield actor.
            let (resp_tx, resp_rx) = oneshot::channel();
            if bitfield_tx.send(BitfieldMsg::IsComplete(resp_tx)).await.is_err() {
                return Err(Error::StartError("Bitfield actor dropped".to_string()));
            }
            if let Ok(true) = resp_rx.await {
                println!("Download complete!");
                break;
            }

            // Fetch peers from tracker and DHT.
            let mut peers = match fetch_peers(&self.torrent, peer_id).await {
                Ok(p) => p,
                Err(e) => {
                    println!("Error fetching peers: {}. Retrying...", e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            println!("Total peers discovered: {}", peers.len());

            if peers.is_empty() {
                println!("No peers found. Retrying in 10 seconds...");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }

            let mut active_workers = FuturesUnordered::new();
            let mut peer_iter = peers.drain(..);

            // Start up to MAX_CONCURRENT_PEERS workers.
            for _ in 0..MAX_CONCURRENT_PEERS {
                if let Some(peer_addr) = peer_iter.next() {
                    let torrent_clone = self.torrent.clone();
                    let bitfield_tx_clone = bitfield_tx.clone();
                    let output_tx_clone = output_tx.clone();
                    let info_hash = info_hash;
                    let peer_id = peer_id;
                    active_workers.push(tokio::spawn(async move {
                        match perform_handshake(&peer_addr, info_hash, peer_id).await {
                            Ok(peer_stream) => {
                                let mut worker = PeerWorker::new(peer_stream, torrent_clone, bitfield_tx_clone, output_tx_clone, peer_addr);
                                if let Err(e) = worker.start_worker().await {
                                    println!("Worker for peer {} failed: {}", peer_addr, e);
                                }
                                println!("Worker for peer {} finished", peer_addr);
                            }
                            Err(e) => {
                                println!("Failed to connect to peer {}: {}", peer_addr, e);
                            }
                        }
                    }));
                }
            }

            // As each worker finishes, spawn a new one until all peers are tried.
            while let Some(_res) = active_workers.next().await {
                if let Some(peer_addr) = peer_iter.next() {
                    let torrent_clone = self.torrent.clone();
                    let bitfield_tx_clone = bitfield_tx.clone();
                    let output_tx_clone = output_tx.clone();
                    let info_hash = info_hash;
                    let peer_id = peer_id;
                    active_workers.push(tokio::spawn(async move {
                        match perform_handshake(&peer_addr, info_hash, peer_id).await {
                            Ok(peer_stream) => {
                                let mut worker = PeerWorker::new(peer_stream, torrent_clone, bitfield_tx_clone, output_tx_clone, peer_addr);
                                if let Err(e) = worker.start_worker().await {
                                    println!("Worker for peer {} failed: {}", peer_addr, e);
                                }
                                println!("Worker for peer {} finished", peer_addr);
                            }
                            Err(e) => {
                                println!("Failed to connect to peer {}: {}", peer_addr, e);
                            }
                        }
                    }));
                }
            }

            println!("All peers have been tried or all workers finished. Checking for completion...");
            // Wait a short time before retrying to avoid hammering trackers.
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        Ok(())
    }
}

impl PeerWorker {
    pub fn new(stream: TcpStream, torrent: TorrentInfo, bitfield_tx: BitfieldSender, output_tx: OutputSender, peer_addr: SocketAddr) -> Self {
        Self {
            stream,
            pending_requests: HashMap::new(),
            torrent,
            bitfield_tx,
            output_tx,
            peer_addr,
            peer_bitfield: None,
        }
    }

    /// The main worker loop. Handles messages from the peer and manages piece requests.
    async fn start_worker(&mut self) -> Result<(), Error> {
        println!("Worker started for peer {}", self.peer_addr);
        let mut choked = true;
        let mut next_piece = None;
        let mut piece_buffer: Vec<u8> = vec![0; self.torrent.piece_length as usize];
        let mut piece_size = self.torrent.piece_length as usize;

        loop {
            let message_result = if choked {
                match tokio::time::timeout(Duration::from_secs(60), read_peer_message(&mut self.stream)).await {
                    Ok(res) => res,
                    Err(_) => {
                        println!("Connection to peer {} timed out", self.peer_addr);
                        if next_piece.is_some() {
                            self.bitfield_tx.send(BitfieldMsg::PieceFailed(next_piece.unwrap() as u32)).await.unwrap();
                        }
                        return Ok(());
                    }
                }
            } else {
                read_peer_message(&mut self.stream).await
            };

            if let Err(e) = &message_result {
                println!("Connection to peer {} lost: {}", self.peer_addr, e);
                if next_piece.is_some() {
                    self.bitfield_tx.send(BitfieldMsg::PieceFailed(next_piece.unwrap() as u32)).await.unwrap();
                }
                return Ok(());
            }
            let (payload_id, payload) = message_result.unwrap();
            println!("Worker {}: Received message type {}", self.peer_addr, payload_id);
            
            match payload_id {
                0 => {
                    // Choke message.
                    println!("Worker {}: Choked", self.peer_addr);
                    choked = true;
                }
                1 => {
                    // Unchoke message.
                    println!("Worker {}: Unchoked", self.peer_addr);
                    choked = false;
                }
                2 => {
                    // Interested message.
                    println!("Worker {}: Peer is interested", self.peer_addr);
                }
                3 => {
                    // Not interested message.
                    println!("Worker {}: Peer not interested", self.peer_addr);
                }
                4 => {
                    println!("Worker {}: Received have message of {} bytes", self.peer_addr, payload.len());
                    let received_index = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
                    // Have message, update bitfield.
                    println!("Worker {}: Peer has piece {}", self.peer_addr, received_index);
                    if next_piece.is_none() {
                        let (resp_tx, _resp_rx) = tokio::sync::oneshot::channel();
                        if self.bitfield_tx.send(BitfieldMsg::RequestPiece(received_index, resp_tx)).await.is_ok() {
                            next_piece = Some(received_index);
                        }
                    }
                }
                5 => {
                    // Bitfield message, update bitfield.
                    println!("Worker {}: Received bitfield of {} bytes", self.peer_addr, payload.len());
                    
                    // Debug: print the raw bitfield bytes.
                    println!("Worker {}: Raw bitfield: {:02x?}", self.peer_addr, &payload[..std::cmp::min(4, payload.len())]);
                    
                    let bitfield = vec_u8_to_box_bool(payload.to_vec());
                    self.peer_bitfield = Some(bitfield);
                    // Debug: count how many pieces the peer has
                    let peer_piece_count = self.peer_bitfield.as_ref().unwrap().iter().filter(|&&x| x).count();
                    println!("Worker {}: Peer has {} out of {} pieces",
                             self.peer_addr, peer_piece_count, self.torrent.pieces.len() / 20);

                    if next_piece.is_none() {
                        next_piece = get_requestable_piece(&self.bitfield_tx, self.peer_bitfield.as_ref().unwrap().clone()).await.unwrap();
                        // Adjust size for the last piece.
                        if next_piece == Some((self.torrent.pieces.len() / 20) - 1) {
                            piece_size = self.torrent.length.unwrap_or(0) as usize % self.torrent.piece_length as usize;
                            piece_buffer = vec![0; piece_size];
                        }
                    }

                    if next_piece.is_none() {
                        println!("Worker {}: No pieces available to request", self.peer_addr);
                        continue;
                    }

                    // TODO: make sure we dont switch pieces if we are mid-piece.

                    // Send the interested message to peer.
                    println!("Worker {}: Sending interested message for piece: {:?}", self.peer_addr, next_piece);
                    const INTERESTED: [u8; 5] = [0, 0, 0, 1, 2];
                    self.stream.write_all(&INTERESTED).await?;
                }
                6 => {
                    // Request message, send piece data.
                    println!("Worker {}: Sending piece data", self.peer_addr);
                }
                7 => {
                    // Verify received block.
                    // First 4 bytes are piece index.
                    let received_index = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    if received_index != next_piece.unwrap_or(usize::MAX) as u32 {
                        println!("Worker {}: Received piece index {} does not match requested piece {}", 
                                 self.peer_addr, received_index, next_piece.unwrap());
                        continue;
                    }
                    // Get begin index.
                    let begin = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
                    // Mark the block as fulfilled.
                    self.pending_requests.insert(begin, true);
                    // Write block into piece buffer.
                    piece_buffer[begin as usize..(begin as usize + payload.len() - 8)].copy_from_slice(&payload[8..]);
                    
                    println!("Worker {}: Received block for piece {}, offset {}, size {}", 
                             self.peer_addr, received_index, begin, payload.len() - 8);

                    if self.received_all_blocks(piece_size) {
                        if self.verify_piece(next_piece.unwrap() as u32, &piece_buffer) {
                            // We have verified the piece, register it as completed.
                            self.bitfield_tx.send(BitfieldMsg::Have(next_piece.unwrap() as usize)).await.unwrap();
                            println!("Successfully downloaded and verified piece {}", next_piece.unwrap());
                            // Send piece to output actor.
                            self.output_tx.send(OutputMsg::Have((next_piece.unwrap(), piece_buffer))).await.unwrap();
                            // Reset for next piece.
                            next_piece = None;
                            piece_buffer = vec![0; self.torrent.piece_length as usize];
                            // Try to request another piece.
                            next_piece = get_requestable_piece(&self.bitfield_tx, self.peer_bitfield.as_ref().unwrap().clone()).await.unwrap();

                        } else {
                            // Piece verification failed, register piece as failed.
                            self.bitfield_tx.send(BitfieldMsg::PieceFailed(next_piece.unwrap() as u32)).await.unwrap();
                            println!("Piece verification failed for piece {}", next_piece.unwrap());
                            if(next_piece.is_some()) {
                                // If we were in the middle of downloading a piece, mark it as failed.
                                self.bitfield_tx.send(BitfieldMsg::PieceFailed(next_piece.unwrap() as u32)).await.unwrap();
                            }
                            return Ok(());
                        }
                    }
                }
                8 => {
                    // Cancel message, ignore for now.
                }
                _ => {
                    // Other message, ignore.
                }
            }

            // Request pieces if possible.
            if !choked && next_piece.is_some() && self.pending_requests.len() < MAX_WORKER_REQUESTS {
                let block_to_request = self.next_block();
                if block_to_request.is_none() {
                    continue;
                }

                if let Err(e) = self.request_block(next_piece.unwrap(), block_to_request.unwrap(), piece_size).await {
                    println!("Worker {}: Failed to request block: {}", self.peer_addr, e);
                    if(next_piece.is_some()) {
                        // If we were in the middle of downloading a piece, mark it as failed.
                        self.bitfield_tx.send(BitfieldMsg::PieceFailed(next_piece.unwrap() as u32)).await.unwrap();
                    }
                    return Ok(());
                }

                // Mark the block as requested.
                self.pending_requests.insert(block_to_request.unwrap() as u32, false);
            }
        }
    }
    /// Check if we have received the entire piece. We do this by checking if all requested blocks are fulfilled.
    /// and by checking whether the number of pending requests matches the piece length.
    fn received_all_blocks(&self, piece_size: usize) -> bool {
        let mut total = 0;
        for (&begin, &fulfilled) in &self.pending_requests {
            if !fulfilled {
                return false;
            }
            // Each block is BLOCK_SIZE except possibly the last one
            let block_len = std::cmp::min(BLOCK_SIZE, piece_size.saturating_sub(begin as usize));
            total += block_len;
        }
        total >= piece_size
    }

    fn verify_piece(&self, index: u32, piece_buffer: &Vec<u8>) -> bool {
        let mut hasher = Sha1::new();
        hasher.update(piece_buffer);
        let result = hasher.finalize();
        let expected = &self.torrent.pieces[(index as usize * 20)..(index as usize * 20 + 20)];
        result.as_slice() == expected
    }
 
    async fn request_block(&mut self, index: usize, begin: usize, piece_size: usize) -> Result<(), Error> {
        let size = std::cmp::min(BLOCK_SIZE as u32, piece_size as u32 - begin as u32);
        // Create request message.
        let mut request = Vec::with_capacity(17);
        request.extend_from_slice(&(13u32.to_be_bytes()));                // length
        request.push(6);                                                  // id
        request.extend_from_slice(&(index as u32).to_be_bytes());         // piece index (4 bytes).
        request.extend_from_slice(&(begin as u32).to_be_bytes());         // begin offset (4 bytes).
        request.extend_from_slice(&(size as u32).to_be_bytes());          // block size (4 bytes).
        self.stream.write_all(&request).await?;

        self.pending_requests.insert(begin as u32, false);

        println!("Worker {}: Requested block at piece {}, offset {}, size {}", 
                 self.peer_addr, index, begin, size);   
        Ok(())
    }

    fn next_block(&self) -> Option<usize> {
        for i in (0..(self.torrent.piece_length)).step_by(BLOCK_SIZE) {
            if !self.pending_requests.contains_key(&(i as u32)) {
                return Some(i as usize);
            }
        }
        None
    }
}

/// Read a message from the peer. Returns the payload id and payload data.
async fn read_peer_message(stream: &mut TcpStream) -> Result<(u8, Vec<u8>), Error> {
    let mut buf4 = [0u8; 4];
    // Read the length prefix.
    stream.read_exact(&mut buf4).await?;
    let size = u32::from_be_bytes(buf4);
    if size == 0 {
        // Keep-alive message.
        return Ok((0, vec![]));
    }

    // Read the payload id.
    let mut payload_id_buffer = [0u8; 1];
    stream.read_exact(&mut payload_id_buffer).await?;
    let payload_id = payload_id_buffer[0];

    // Read the payload data.
    let mut payload = vec![0u8; (size - 1) as usize];
    stream.read_exact(&mut payload).await?;

    Ok((payload_id, payload))
}

/// Create the BitTorrent handshake message.
pub fn create_handshake(info_hash: [u8; 20], peer_id: [u8; 20]) -> [u8; 68] {
    let mut handshake = [0u8; 68];
    handshake[0] = 19; 
    handshake[1..20].copy_from_slice(b"BitTorrent protocol"); 
    handshake[20..28].copy_from_slice(&[0u8; 8]); // reserved
    handshake[28..48].copy_from_slice(&info_hash); // info_hash.
    handshake[48..68].copy_from_slice(&peer_id); // peer_id.
    handshake
}

/// Perform the BitTorrent handshake with a peer.
pub async fn perform_handshake(peer: &SocketAddr, info_hash: [u8; 20], peer_id: [u8; 20]) -> Result<TcpStream, Error> {
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

/// Generate a random peer ID.
pub fn get_client_id() -> [u8; 20] {
    let mut peer_id = [0u8; 20];
    peer_id[..8].copy_from_slice(CLIENT_ID.as_bytes());
    rand::rng().fill_bytes(&mut peer_id[8..]);
    peer_id
}

/// Fetch peers from the tracker.
pub async fn fetch_peers(torrent_info: &TorrentInfo, id: [u8; 20]) -> Result<Vec<SocketAddr>, Error> {
    use futures::future::join_all;
    let client = Client::new();
    let mut tasks = Vec::new();

    for announce in &torrent_info.announce_list {
        let announce = announce.clone();
        let torrent_info = torrent_info.clone();
        let id = id.clone();
        let client = client.clone();

        // Spawn a task for each announce URL.
        tasks.push(tokio::spawn(async move {
            let mut peers: Vec<SocketAddr> = Vec::new();
            // Check if it's UDP or HTTP tracker.
            if announce.starts_with("udp://") {
                // UDP tracker protocol.
                // Resolve the hostname.
                let socket_addr_str = announce.trim_start_matches("udp://").split('/').next().unwrap_or("");
                let mut addrs_iter = match tokio::net::lookup_host(socket_addr_str).await {
                    Ok(iter) => iter,
                    Err(_) => return peers,
                };
                let resolved_addr = match addrs_iter.next() {
                    Some(addr) => addr,
                    None => return peers,
                };
                let socket = match UdpSocket::bind("0.0.0.0:0").await {
                    Ok(s) => s,
                    Err(_) => return peers,
                };

                // Construct connection request.
                let mut conn_request = [0u8; 16];
                conn_request[..8].copy_from_slice(&0x41727101980u64.to_be_bytes());
                conn_request[8..12].copy_from_slice(&0u32.to_be_bytes());
                let conn_transaction_id: u32 = rand::random();
                conn_request[12..16].copy_from_slice(&conn_transaction_id.to_be_bytes());
                let _ = socket.send_to(&conn_request, resolved_addr).await;

                // Send and receive connection response.
                let mut buf = [0u8; 512];
                let (len, _) = match timeout(Duration::from_secs(3), socket.recv_from(&mut buf)).await {
                    Ok(Ok(res)) => res,
                    _ => return peers,
                };
                if len < 16 { return peers; }

                // Parse connection response.
                let action = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
                let transaction_id = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
                if action != 0 || transaction_id != conn_transaction_id { return peers; }
                let connection_id = &buf[8..16];

                // Construct announce request.
                let mut announce_req = Vec::with_capacity(98);
                announce_req.extend_from_slice(connection_id);
                announce_req.extend_from_slice(&1u32.to_be_bytes());
                let announce_transaction_id: u32 = rand::random();
                announce_req.extend_from_slice(&announce_transaction_id.to_be_bytes());
                announce_req.extend_from_slice(&torrent_info.info_hash);
                announce_req.extend_from_slice(&id);
                announce_req.extend_from_slice(&0u64.to_be_bytes());
                let left = torrent_info.length.unwrap_or(0);
                announce_req.extend_from_slice(&(left as u64).to_be_bytes());
                announce_req.extend_from_slice(&0u64.to_be_bytes());
                announce_req.extend_from_slice(&0u32.to_be_bytes());
                announce_req.extend_from_slice(&0u32.to_be_bytes());
                let key: u32 = rand::random();
                announce_req.extend_from_slice(&key.to_be_bytes());
                announce_req.extend_from_slice(&(-1i32).to_be_bytes());
                announce_req.extend_from_slice(&DEFAULT_PORT.to_be_bytes());

                // Send announce request and receive response.
                let _ = socket.send_to(&announce_req, resolved_addr).await;
                let mut buf2 = [0u8; 4096];
                let (len2, _) = match timeout(Duration::from_secs(3), socket.recv_from(&mut buf2)).await {
                    Ok(Ok(res)) => res,
                    _ => return peers,
                };
                // Parse announce response.
                if len2 < 20 { return peers; }
                let action2 = u32::from_be_bytes([buf2[0], buf2[1], buf2[2], buf2[3]]);
                if action2 != 1 { return peers; }
                let peers_bytes = &buf2[20..len2];
                for chunk in peers_bytes.chunks_exact(6) {
                    let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
                    let port = u16::from_be_bytes([chunk[4], chunk[5]]);
                    let peer = SocketAddr::new(ip.into(), port);
                    peers.push(peer);
                }
                println!("UDP tracker {} returned {} peers", announce, peers.len());
            } else if announce.starts_with("http://") || announce.starts_with("https://") {
                // Construct HTTP GET request.
                let url = format!(
                    "{}?info_hash={}&peer_id={}&port={}&uploaded=0&downloaded=0&left={}&compact=1",
                    announce,
                    urlencoding::encode_binary(&torrent_info.info_hash),
                    urlencoding::encode_binary(&id),
                    DEFAULT_PORT,
                    torrent_info.length.unwrap_or(0)
                );
                // Send GET request.
                let response = match client.get(&url).send().await {
                    Ok(resp) => match resp.bytes().await { Ok(b) => b, Err(_) => return peers },
                    Err(_) => return peers,
                };
                // Parse bencoded response.
                let bencode = match BencodeRef::decode(&response, BDecodeOpt::default()) {
                    Ok(b) => b,
                    Err(_) => return peers,
                };
                let dict = match bencode.dict() {
                    Some(d) => d,
                    None => return peers,
                };
                let peers_bytes = match dict.lookup(b"peers").and_then(|p| p.bytes()) {
                    Some(p) => p,
                    None => return peers,
                };
                // Parse peers to list.
                for chunk in peers_bytes.chunks_exact(6) {
                    let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
                    let port = u16::from_be_bytes([chunk[4], chunk[5]]);
                    let peer = SocketAddr::new(ip.into(), port);
                    peers.push(peer);
                }
            }
            peers
        }));
    }

    let results = join_all(tasks).await;
    let mut all_peers: Vec<SocketAddr> = Vec::new();
    let mut seen: std::collections::HashSet<SocketAddr> = std::collections::HashSet::new();
    for res in results {
        if let Ok(peers) = res {
            for peer in peers {
                if seen.contains(&peer) { continue; }
                seen.insert(peer);
                all_peers.push(peer);
            }
        }
    }

    // Add DHT nodes if available.
    let dht_peers = dht_search(torrent_info.clone(), String::from_utf8_lossy(&id).into_owned()).await;
    for peer in dht_peers {
        if seen.contains(&peer) {
            continue;
        }
        seen.insert(peer);
        all_peers.push(peer);
    }
    Ok(all_peers)
}

pub fn create_udp_request(info_hash: &[u8; 20], peer_id: &[u8; 20]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(98);
    // Connection ID (8 bytes).
    buf.extend_from_slice(&0x41727101980u64.to_be_bytes());
    // Action (4 bytes) - 0 for connect.
    buf.extend_from_slice(&0u32.to_be_bytes());
    // Transaction ID (4 bytes) - random.
    let transaction_id: u32 = rand::random();
    buf.extend_from_slice(&transaction_id.to_be_bytes());
    // Info hash (20 bytes).
    buf.extend_from_slice(info_hash);
    // Peer ID (20 bytes).
    buf.extend_from_slice(peer_id);
    // Downloaded (8 bytes).
    buf.extend_from_slice(&0u64.to_be_bytes());
    // Left (8 bytes).
    buf.extend_from_slice(&0u64.to_be_bytes());
    // Uploaded (8 bytes).
    buf.extend_from_slice(&0u64.to_be_bytes());
    // Event (4 bytes) - 0 for none.
    buf.extend_from_slice(&0u32.to_be_bytes());
    // IP address (4 bytes) - 0 for default.
    buf.extend_from_slice(&0u32.to_be_bytes());
    // Key (4 bytes) - random.
    let key: u32 = rand::random();
    buf.extend_from_slice(&key.to_be_bytes());
    // Num want (4 bytes) - -1 for default.
    buf.extend_from_slice(&(-1i32).to_be_bytes());
    // Port (2 bytes).
    buf.extend_from_slice(&DEFAULT_PORT.to_be_bytes());
    
    buf
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