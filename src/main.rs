mod torrent;

use std::env;


use crate::torrent::{TorrentInfo, parse_torrent_file};

fn main() {
    let args: Vec<String> = env::args().collect();
    // Check if the user provided a path to a .torrent file.
    if args.len() < 2 {
        eprintln!("Usage: {} <path to .torrent file>", args[0]);
        return;
    }
    let torrent_path = &args[1];
    match parse_torrent_file(torrent_path) {
        Ok(info) => {
            println!("Torrent Info:");
            println!("  Announce: {}", info.announce);
            println!("  Name: {}", info.name);
            println!("  Piece Length: {}", info.piece_length);
            if let Some(length) = info.length {
                println!("  Length: {}", length);
            }
            println!("  Pieces: {:?}", info.pieces);
        }
        Err(e) => eprintln!("Error: {}", e),
    }
}

