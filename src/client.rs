use std::net::SocketAddr;
use tokio::net::TcpStream;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum ClientError {
    #[error("Error with starting the client: {0}")]
    StartError(String),
}

pub struct TorrentClient {
    pub torrent: crate::torrent::TorrentInfo,
}

impl TorrentClient {
    pub fn new(torrent: crate::torrent::TorrentInfo) -> Self {
        Self { torrent }
    }

    pub async fn start(&self) -> Result<(), ClientError> {
        // Todo...
        Ok(())
    }
}