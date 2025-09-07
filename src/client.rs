use sha1::{Sha1, Digest};
use tokio::{io::AsyncWriteExt, io::AsyncReadExt, net::TcpStream};

use thiserror::Error;
use reqwest::Client;

use rand::{Fill, RngCore};
use hex::ToHex;

use std::fs::write;

use bip_bencode::{BDecodeOpt, BRefAccess, BencodeRef};

use urlencoding;

use std::net::{SocketAddr, Ipv4Addr};

use crate::torrent::{TorrentInfo, get_torrent_info_hash};

const DEFAULT_PORT: u16 = 6881;
const CLIENT_ID : &str = "-BF0001-"; // BitFrost client identifier
const BLOCK_SIZE : usize = 16384; // 16KB block size

pub struct TorrentClient {
    torrent: TorrentInfo,
    peer_id : [u8; 20],
}

impl TorrentClient {
    pub fn new(torrent: TorrentInfo) -> Self {
        Self { torrent, peer_id: [0; 20]}
    }

    pub async fn start(&mut self) -> Result<(), Error> {
        self.peer_id = get_client_id(); 
        println!("Starting torrent client for: {}", self.torrent.name);
        println!("Info Hash: {:x?}", self.torrent.info_hash);
        println!("Getting peers from tracker...");
        self.download_piece(0).await?;
        Ok(())
    }

    async fn download_piece(&self, index: u32) -> Result<(), Error> {
        let peers = fetch_peers(&self.torrent, self.peer_id).await?;
        let mut stream = perform_handshake(peers.iter().next().unwrap(), self.torrent.info_hash, self.peer_id).await?;
        let mut piece_buffer: Vec<u8> = vec![0; self.torrent.piece_length as usize];

        // Read bitfield message.
        let size = stream.read_u32().await?;
        let _payload_id = stream.read_u8().await?;
        let mut bitfield = vec![0u8; (size - 1) as usize];
        stream.read_exact(&mut bitfield).await?;

        // Send the interested message to peer.
        // size is 0001, id is 2.
        const INTERESTED: [u8; 5] = [0, 0, 0, 1, 2];
        stream.write_all(&INTERESTED).await?;

        // Read the unchoke message from peer.
        let _size = stream.read_u32().await?;
        let _payload_id = stream.read_u8().await?;
        if _payload_id != 1 {
            return Err(Error::StartError("Expected unchoke message".into()));
        }

        // Now request the piece in BLOCK_SIZE chunks.
        for begin in (0..(self.torrent.piece_length)).step_by(BLOCK_SIZE) {
            let size = std::cmp::min(BLOCK_SIZE as u32, self.torrent.piece_length as u32 - begin as u32);
            // Request the piece block from the peer.
            let mut request = Vec::with_capacity(17);
            request.extend_from_slice(&(13u32.to_be_bytes())); // length
            request.push(6); // request ID
            request.extend_from_slice(&index.to_be_bytes());
            request.extend_from_slice(&(begin as u32).to_be_bytes());
            request.extend_from_slice(&size.to_be_bytes());

            let length = u32::from_be_bytes([request[0], request[1], request[2], request[3]]);
            let id = request[4];
            let idx = u32::from_be_bytes([request[5], request[6], request[7], request[8]]);
            let begin_val = u32::from_be_bytes([request[9], request[10], request[11], request[12]]);
            let size_val = u32::from_be_bytes([request[13], request[14], request[15], request[16]]);

            println!("Request fields:");
            println!("  length: {}", length);
            println!("  id: {}", id);
            println!("  index: {}", idx);
            println!("  begin: {}", begin_val);
            println!("  size: {}", size_val);

            stream.write_all(&request).await?;

            // Read the piece message from the peer.
            let mut buf4 = [0u8; 4];

            // Length.
            stream.read_exact(&mut buf4).await?;
            let received_size = u32::from_be_bytes(buf4);
            if received_size != size + 9 {
                return Err(Error::PieceError(format!("Unexpected piece size: {}, expected: {}", received_size, size + 9)));
            }
            
            // Payload id.
            let mut payload_id_buffer = [0u8; 1];
            stream.read_exact(&mut payload_id_buffer).await?;
            let payload_id = payload_id_buffer[0];
            if payload_id != 7 {
                return Err(Error::PieceError("Expected piece message".into()));
            }
            
            // Piece index.
            stream.read_exact(&mut buf4).await?;
            let received_index = u32::from_be_bytes(buf4);
            if received_index != index {
                return Err(Error::PieceError(format!("Unexpected piece index: {}", received_index)));
            }

            stream.read_exact(&mut buf4).await?;
            let begin = u32::from_be_bytes(buf4);
            if begin != begin_val {
                return Err(Error::PieceError(format!("Unexpected begin offset: {}", begin)));
            }

            // Read data block.
            stream.read_exact(&mut piece_buffer[begin as usize..(begin as usize + size as usize)]).await?;
        }

        // verify hash.
        let mut hasher = Sha1::new();
        hasher.update(&piece_buffer);
        let result = hasher.finalize();
        let expected = &self.torrent.pieces[(index as usize * 20)..(index as usize * 20 + 20)];

        if result.as_slice() != expected {
            return Err(Error::PieceError("Piece hash mismatch".into()));
        }

        std::fs::write("./out.txt", &piece_buffer).map_err(|e| Error::PieceError(format!("Failed to write piece to file: {}", e)))?;
        
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
    let bencode = BencodeRef::decode(&response, BDecodeOpt::default())
        .map_err(|e| Error::StartError(format!("Bencode decoding error: {}", e)))?;

    let dict = bencode.dict()
        .ok_or_else(|| Error::StartError("Expected dictionary response".into()))?;

    let peers_bytes = dict.lookup(b"peers")
    .and_then(|p| p.bytes())
    .ok_or_else(|| Error::StartError("Missing 'peers' field".into()))?;

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

    #[error("Error with downloading piece: {0}")]
    PieceError(String),
}