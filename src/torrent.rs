
use bendy::decoding::{FromBencode, Decoder, Object};
use bendy::value::Value;

use std::fs;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum TorrentError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("Bencode error: {0}")]
    Bencode(String),

    #[error("Invalid torrent file: {0}")]
    InvalidTorrent(String),
}

pub struct TorrentInfo {
    pub announce: String,
    pub name: String,
    pub piece_length: u64,
    pub length: Option<u64>,
    pub pieces: Vec<u8>,
}

pub fn parse_torrent_file(path: &str) -> Result<TorrentInfo, TorrentError> {
    let data = std::fs::read(path)?; 
    let val = bendy::value::Value::from_bencode(&data)
    .map_err(|e| TorrentError::Bencode(format!("Bencode decoding error: {}", e)))?;

    let dict = match val {
        bendy::value::Value::Dict(d) => d,
        _ => return Err(TorrentError::InvalidTorrent("Top-level is not a dictionary".into())),
    };

    let announce = match dict.get(&b"announce"[..]) {
        Some(bendy::value::Value::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
        _ => return Err(TorrentError::InvalidTorrent("Missing 'announce'".into())),
    };

    let info = match dict.get(&b"info"[..]) {
        Some(bendy::value::Value::Dict(d)) => d,
        _ => return Err(TorrentError::InvalidTorrent("Missing 'info' dictionary".into())),
    };

    let name = match info.get(&b"name"[..]) {
        Some(bendy::value::Value::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
        _ => return Err(TorrentError::InvalidTorrent("Missing 'name'".into())),
    };

    let piece_length = match info.get(&b"piece length"[..]) {
        Some(bendy::value::Value::Integer(i)) => *i as u64,
        _ => return Err(TorrentError::InvalidTorrent("Missing 'piece length'".into())),
    };

    let length = match info.get(&b"length"[..]) {
        Some(bendy::value::Value::Integer(i)) => Some(*i as u64),
        _ => None,
    };

    let pieces = match info.get(&b"pieces"[..]) {
        Some(bendy::value::Value::Bytes(b)) => b.to_vec(),
        _ => return Err(TorrentError::InvalidTorrent("Missing 'pieces'".into())),
    };

    Ok(TorrentInfo {
        announce,
        name,
        piece_length,
        length,
        pieces,
    })
}
