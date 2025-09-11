
use bendy::decoding::{FromBencode};
use bendy::value::Value;

use thiserror::Error;

#[derive(Clone)]
pub struct TorrentInfo {
    pub announce_list: Vec<String>,
    pub nodes: Vec<(String, u16)>, // (host, port)
    pub name: String,
    pub piece_length: u64,
    pub length: Option<u64>,
    pub pieces: Vec<u8>,
    pub info_hash: [u8; 20],
}

// Parse a .torrent file and return a TorrentInfo struct.
pub fn parse_torrent_file(path: &str) -> Result<TorrentInfo, Error> {
    let data = std::fs::read(path)?; 
    let val = bendy::value::Value::from_bencode(&data)
    .map_err(|e| Error::Bencode(format!("Bencode decoding error: {}", e)))?;

    let dict = match val {
        bendy::value::Value::Dict(d) => d,
        _ => return Err(Error::InvalidTorrent("Top-level is not a dictionary".into())),
    };

    let mut announce_list = Vec::new();
    let announce = match dict.get(&b"announce"[..]) {
        Some(bendy::value::Value::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
        _ => String::new(),
    };
    if announce != "" {
        announce_list.push(announce.clone());
    }
    
    if let Some(Value::List(tiers)) = dict.get(&b"announce-list"[..]) {
        for tier in tiers {
            let trackers = match tier {
                Value::List(trackers) => trackers,
                _ => continue,
            };
            for tracker in trackers {
                let url = match tracker {
                    Value::Bytes(url) => url,
                    _ => continue,
                };
                announce_list.push(String::from_utf8_lossy(url).into_owned());
            }
        }
    }

    // Parse nodes (for DHT bootstrap).
    let mut nodes = Vec::new();
    if let Some(Value::List(node_list)) = dict.get(&b"nodes"[..]) {
        for node in node_list {
            let pair = match node {
                Value::List(pair) if pair.len() == 2 => pair,
                _ => continue,
            };
            let host = match &pair[0] {
                Value::Bytes(h) => h,
                _ => continue,
            };
            let port = match &pair[1] {
                Value::Integer(p) => *p,
                _ => continue,
            };
            if let Ok(host_str) = String::from_utf8(host.to_vec()) {
                nodes.push((host_str, port as u16));
            }
        }
    }

    println!("Announce list: {:?}", announce_list);
    println!("DHT nodes: {:?}", nodes);

    let info = match dict.get(&b"info"[..]) {
        Some(bendy::value::Value::Dict(d)) => d,
        _ => return Err(Error::InvalidTorrent("Missing 'info' dictionary".into())),
    };

    let name = match info.get(&b"name"[..]) {
        Some(bendy::value::Value::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
        _ => return Err(Error::InvalidTorrent("Missing 'name'".into())),
    };

    let piece_length = match info.get(&b"piece length"[..]) {
        Some(bendy::value::Value::Integer(i)) => *i as u64,
        _ => return Err(Error::InvalidTorrent("Missing 'piece length'".into())),
    };

    let length = match info.get(&b"length"[..]) {
        Some(bendy::value::Value::Integer(i)) => Some(*i as u64),
        _ => None,
    };

    let pieces = match info.get(&b"pieces"[..]) {
        Some(bendy::value::Value::Bytes(b)) => b.to_vec(),
        _ => return Err(Error::InvalidTorrent("Missing 'pieces'".into())),
    };

    println!("Parsed torrent file: {}", name);
    println!("Announce URL: {}", announce);
    println!("Piece Length: {}", piece_length);
    if let Some(len) = length {
        println!("Total Length: {}", len);
    }
    println!("Number of Pieces: {}", pieces.len() / 20);
    println!("Pieces Hashes: {:x?}", &pieces[..std::cmp::min(60, pieces.len())]);

    Ok(TorrentInfo {
        announce_list,
        nodes,
        name,
        piece_length,
        length,
        pieces,
        info_hash: get_torrent_info_hash(&Value::Dict(info.clone()))?,
    })
}

pub fn get_torrent_info_hash(info: &Value) -> Result<[u8; 20], Error> {
    use sha1::{Sha1, Digest};
    use bendy::encoding::ToBencode;

    let info_bencode = info.to_bencode().map_err(|e| Error::Bencode(format!("Bencode error: {}", e)))?;
    let mut hasher = Sha1::new();
    hasher.update(&info_bencode);
    let result = hasher.finalize();
    let mut info_hash = [0u8; 20];
    info_hash.copy_from_slice(&result);
    Ok(info_hash)
}

#[derive(Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("Bencode error: {0}")]
    Bencode(String),

    #[error("Invalid torrent file: {0}")]
    InvalidTorrent(String),
}