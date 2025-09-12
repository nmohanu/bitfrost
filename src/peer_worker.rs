use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use sha1::{digest, Sha1};
use digest::Digest;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use crate::torrent::TorrentInfo;
use crate::bitfield_actor::{BitfieldSender, BitfieldMsg};
use crate::output_actor::{OutputSender, OutputMsg};

use crate::util::{vec_u8_to_box_bool};
use crate::bitfield_actor::get_requestable_piece;

const BLOCK_SIZE : usize = 16384; // 16KB block size.
const MAX_WORKER_REQUESTS: usize = 5; // Max pending requests per worker.
const MAX_PENDING_DURATION: u64 = 120; // Max duration (in seconds) to wait for pending requests.

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
    choked: bool,
    next_piece: Option<usize>,
    piece_buffer: Vec<u8>,
    piece_size: usize,
}


impl PeerWorker {
    pub fn new(stream: TcpStream, torrent: TorrentInfo, bitfield_tx: BitfieldSender, output_tx: OutputSender, peer_addr: SocketAddr) -> Self {
        let piece_len = torrent.piece_length as usize;
        Self {
            stream,
            pending_requests: HashMap::new(),
            torrent,
            bitfield_tx,
            output_tx,
            peer_addr,
            peer_bitfield: None,
            choked: true,
            next_piece: None,
            piece_buffer: vec![0; piece_len],
            piece_size: piece_len,
        }
    }

    /// The main worker loop. Handles messages from the peer and manages piece requests.
    pub async fn start_worker(&mut self) -> Result<(), Error> {
        println!("Worker started for peer {}", self.peer_addr);

        // Send bitfield message to peer if we have pieces.
        self.send_bitfield().await;

        loop {
            // If worker is choked, set a timeout for reading messages.
            // If unchoked, read and process messages.
            // If timed out, we register the piece as failed.
            let message_result = if self.choked {
                match tokio::time::timeout(Duration::from_secs(MAX_PENDING_DURATION), self.read_peer_message()).await {
                    Ok(res) => res,
                    Err(_) => {
                        self.terminate("Connection timed out").await;
                        return Ok(());
                    }
                }
            } else {
                self.read_peer_message().await
            };

            if let Err(e) = &message_result {
                self.terminate(&format!("Connection lost {}", e)).await;
                return Ok(());
            }

            // Unwrap the message result.
            let (payload_id, payload) = message_result.unwrap();
            
            match payload_id {
                0 => {
                    // Choke message.
                    println!("Worker {}: Choked", self.peer_addr);
                    self.choked = true;
                }
                1 => {
                    // Unchoke message.
                    println!("Worker {}: Unchoked", self.peer_addr);
                    self.choked = false;
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
                    let received_index = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
                    // Have message, update bitfield.
                    println!("Worker {}: Peer has piece {}", self.peer_addr, received_index);
                    if self.next_piece.is_none() {
                        let (resp_tx, _resp_rx) = tokio::sync::oneshot::channel();
                        if self.bitfield_tx.send(BitfieldMsg::RequestPiece(received_index, resp_tx)).await.is_ok() {
                            self.next_piece = Some(received_index);
                            // Adjust size for the last piece
                            let last_piece = (self.torrent.pieces.len() / 20) - 1;
                            if received_index == last_piece {
                                let total_len = self.torrent.length.unwrap_or(0) as usize;
                                let piece_len = self.torrent.piece_length as usize;
                                let last_piece_size = total_len - (piece_len * last_piece);
                                self.piece_size = last_piece_size;
                                self.piece_buffer = vec![0; last_piece_size];
                            } else {
                                self.piece_size = self.torrent.piece_length as usize;
                                self.piece_buffer = vec![0; self.piece_size];
                            }
                        }
                    }
                }
                5 => {
                    // Bitfield message, update bitfield.
                    println!("Worker {}: Received bitfield of {} bytes", self.peer_addr, payload.len());
                    
                    let bitfield = vec_u8_to_box_bool(payload.to_vec());
                    self.peer_bitfield = Some(bitfield);
                    // Debug: count how many pieces the peer has
                    let peer_piece_count = self.peer_bitfield.as_ref().unwrap().iter().filter(|&&x| x).count();
                    println!("Worker {}: Peer has {} out of {} pieces",
                             self.peer_addr, peer_piece_count, self.torrent.pieces.len() / 20);

                    // If we don't have a piece to request, try to get one.
                    if let Some(idx) = get_requestable_piece(&self.bitfield_tx, self.peer_bitfield.as_ref().unwrap().clone()).await.unwrap() {
                        self.next_piece = Some(idx);
                        // Adjust size for the last piece
                        let last_piece = (self.torrent.pieces.len() / 20) - 1;
                        if idx == last_piece {
                            let total_len = self.torrent.length.unwrap_or(0) as usize;
                            let piece_len = self.torrent.piece_length as usize;
                            let last_piece_size = total_len - (piece_len * last_piece);
                            self.piece_size = last_piece_size;
                            self.piece_buffer = vec![0; last_piece_size];
                        } else {
                            self.piece_size = self.torrent.piece_length as usize;
                            self.piece_buffer = vec![0; self.piece_size];
                        }
                    } else {
                        continue;
                    }

                    // Send the interested message to peer.
                    println!("Worker {}: Sending interested message for piece: {:?}", self.peer_addr, self.next_piece);
                    const INTERESTED: [u8; 5] = [0, 0, 0, 1, 2];
                    self.stream.write_all(&INTERESTED).await?;
                }
                6 => {
                    // Request message, send piece data.
                    println!("Worker {}: Sending piece data", self.peer_addr);
                }
                7 => {
                    // Verify received block.
                    let received_index = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    if received_index != self.next_piece.unwrap_or(usize::MAX) as u32 {
                        println!("Worker {}: Received piece index {} does not match requested piece {}", 
                                 self.peer_addr, received_index, self.next_piece.unwrap());
                        continue;
                    }
                    // Get begin index.
                    let begin = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
                    // Mark the block as fulfilled and write it into the piece buffer.
                    self.pending_requests.insert(begin, true);
                    self.piece_buffer[begin as usize..(begin as usize + payload.len() - 8)].copy_from_slice(&payload[8..]);
                    
                    println!("Worker {}: Received block for piece {}, offset {}, size {}", 
                             self.peer_addr, received_index, begin, payload.len() - 8);

                    if !self.received_all_blocks(self.piece_size) {
                        continue;
                    }  
                    if self.verify_piece(self.next_piece.unwrap() as u32, &self.piece_buffer) {
                        // We have verified the piece, register it as completed.
                        self.bitfield_tx.send(BitfieldMsg::Have(self.next_piece.unwrap() as usize)).await.unwrap();
                        println!("Successfully downloaded and verified piece {}", self.next_piece.unwrap());
                        // Send piece to output actor.
                        self.output_tx.send(OutputMsg::Have((self.next_piece.unwrap(), self.piece_buffer.clone()))).await.unwrap();
                        // Reset state for next piece
                        self.pending_requests.clear();
                        // Try to request another piece using same peer bitfield.
                        let mut found_piece = false;
                        for _ in 0..5 {
                            if let Some(idx) = get_requestable_piece(&self.bitfield_tx, self.peer_bitfield.as_ref().unwrap().clone()).await.unwrap() {
                                self.next_piece = Some(idx);
                                // Adjust size for the last piece
                                let last_piece = (self.torrent.pieces.len() / 20) - 1;
                                if idx == last_piece {
                                    let total_len = self.torrent.length.unwrap_or(0) as usize;
                                    let piece_len = self.torrent.piece_length as usize;
                                    let last_piece_size = total_len - (piece_len * last_piece);
                                    self.piece_size = last_piece_size;
                                    self.piece_buffer = vec![0; last_piece_size];
                                } else {
                                    self.piece_size = self.torrent.piece_length as usize;
                                    self.piece_buffer = vec![0; self.piece_size];
                                }
                                found_piece = true;
                                break;
                            } else {
                                // No piece available, sleep and retry
                                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            }
                        }
                        if !found_piece {
                            // Before exiting, check if download is complete
                            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                            let _ = self.bitfield_tx.send(crate::bitfield_actor::BitfieldMsg::IsComplete(resp_tx)).await;
                            if let Ok(true) = resp_rx.await {
                                println!("Worker {}: Download complete, exiting.", self.peer_addr);
                                return Ok(());
                            } else {
                                println!("Worker {}: No piece available, retrying.", self.peer_addr);
                                // Sleep and retry loop
                                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                                continue;
                            }
                        }
                    } else {
                        self.terminate("Piece verification failed").await;
                        return Ok(());
                    }
                }
                8 => {
                    // Cancel message, ignore for now.
                }
                _ => {
                    // Other message, ignore.
                }
            }

            // Request piece if possible.
            if !self.choked && self.next_piece.is_some() && self.pending_requests.values().filter(|v| !**v).count() < MAX_WORKER_REQUESTS {
                if let Some(block_to_request) = self.next_block(self.piece_size) {
                    if let Err(e) = self.request_block(self.next_piece.unwrap(), block_to_request, self.piece_size).await {
                        self.terminate(&format!("Failed to request block: {}", e)).await;
                        return Ok(());
                    }
                    // Mark the block as requested.
                    self.pending_requests.insert(block_to_request as u32, false);
                }
            }
        }
    }

    async fn send_bitfield(&mut self) {
        // ?Get local bitfield.
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        self.bitfield_tx.send(BitfieldMsg::Get(resp_tx)).await.unwrap();
        let local_bitfield = resp_rx.await.unwrap();

        // Generate bitfield message.
        let bitfield_bytes_len = (local_bitfield.len() + 7) / 8;
        let mut bitfield_bytes = vec![0u8; bitfield_bytes_len];
        for (i, &has_piece) in local_bitfield.iter().enumerate() {
            if has_piece {
                bitfield_bytes[i / 8] |= 1 << (7 - (i % 8));
            }
        }
        let mut message = Vec::with_capacity(1 + bitfield_bytes_len + 4);
        message.extend_from_slice(&((1 + bitfield_bytes_len) as u32).to_be_bytes()); // length
        message.push(5); // id
        message.extend_from_slice(&bitfield_bytes); // bitfield
        self.stream.write_all(&message).await.expect("Failed to send bitfield message");
        println!("Worker {}: Sent bitfield message with {} pieces", self.peer_addr, local_bitfield.iter().filter(|&&x| x).count());
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

    fn next_block(&self, piece_size: usize) -> Option<usize> {
        for i in (0..piece_size).step_by(BLOCK_SIZE) {
            if !self.pending_requests.contains_key(&(i as u32)) {
                return Some(i as usize);
            }
        }
        None
    }

    async fn terminate(&mut self, reason: &str) {
        println!("Terminating worker for peer {}: {}", self.peer_addr, reason);
        let _ = self.bitfield_tx.send(BitfieldMsg::PieceFailed(self.next_piece)).await;
    }
    
    /// Read a message from the peer. Returns the payload id and payload data.
    async fn read_peer_message(&mut self) -> Result<(u8, Vec<u8>), Error> {
        let mut buf4 = [0u8; 4];
        // Read the length prefix.
        self.stream.read_exact(&mut buf4).await?;
        let size = u32::from_be_bytes(buf4);
        if size == 0 {
            // Keep-alive message.
            println!("Received keep-alive message from peer {}", self.peer_addr);
            return Ok((0, vec![]));
        }

        // Read the payload id.
        let mut payload_id_buffer = [0u8; 1];
        self.stream.read_exact(&mut payload_id_buffer).await?;
        let payload_id = payload_id_buffer[0];

        // Read the payload data.
        let mut payload = vec![0u8; (size - 1) as usize];
        self.stream.read_exact(&mut payload).await?;

        Ok((payload_id, payload))
    }
}

#[derive(Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Connection(#[from] reqwest::Error),

    #[error(transparent)]
    Handshake(#[from] std::io::Error),
}