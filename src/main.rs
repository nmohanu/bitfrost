mod torrent;
mod client;

use std::env;

use crate::torrent::{TorrentInfo, parse_torrent_file};
use crate::client::TorrentClient;

use thiserror::Error;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <path to .torrent file>", args[0]);
        return Ok(());
    }

    // Parse the torrent file.
    let torrent_path = &args[1];
    let torrent: TorrentInfo = parse_torrent_file(torrent_path)?;

    // Start the client.
    let client: TorrentClient = TorrentClient::new(torrent);
    client.start().await?;

    Ok(())
}
