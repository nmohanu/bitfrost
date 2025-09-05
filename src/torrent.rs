
use bendy::decoding::{FromBencode, Decoder, Object};
use bendy::value::Value;

use std::fs;

pub struct TorrentInfo {
    pub announce: String,
    pub name: String,
    pub piece_length: u64,
    pub length: Option<u64>,
    pub pieces: Vec<u8>,
}

pub fn parse_torrent_file(path: &str) -> Result<TorrentInfo, Box<dyn std::error::Error>> {
    let data = fs::read(path)?;
    let val = Value::from_bencode(&data)
        .map_err(|e| format!("Failed to decode bencode: {}", e))?;

    // Top-level dict.
    let dict = match val {
        Value::Dict(d) => d,
        _ => return Err("Top-level bencode is not a dictionary".into()),
    };

    // Announce URL.
    let announce = match dict.get(&b"announce"[..]) {
        Some(Value::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
        _ => return Err("Missing 'announce'".into()),
    };

    // Info dict.
    let info = match dict.get(&b"info"[..]) {
        Some(Value::Dict(d)) => d,
        _ => return Err("Missing 'info' dictionary".into()),
    };

    // Name.
    let name = match info.get(&b"name"[..]) {
        Some(Value::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
        _ => return Err("Missing 'name'".into()),
    };

    // Piece length.
    let piece_length = match info.get(&b"piece length"[..]) {
        Some(Value::Integer(i)) => *i as u64,
        _ => return Err("Missing 'piece length'".into()),
    };

    // Total length (single-file).
    let length = match info.get(&b"length"[..]) {
        Some(Value::Integer(i)) => Some(*i as u64),
        _ => None,
    };

    // Pieces.
    let pieces = match info.get(&b"pieces"[..]) {
        Some(Value::Bytes(b)) => b.to_vec(),
        _ => return Err("Missing 'pieces'".into()),
    };

    Ok(TorrentInfo {
        announce,
        name,
        piece_length,
        length,
        pieces,
    })
}