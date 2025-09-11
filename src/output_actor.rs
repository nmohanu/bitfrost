
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use crate::torrent::TorrentInfo;

pub type OutputReceiver = tokio::sync::mpsc::Receiver<OutputMsg>;
pub type OutputSender = tokio::sync::mpsc::Sender<OutputMsg>;

#[derive(Debug)]
pub enum OutputMsg {
    // Signal the output actor to write a piece to disk.
    Have((usize, Vec<u8>)), // (piece index, piece data)
}

pub async fn output_actor(mut rx: OutputReceiver, torrent: TorrentInfo) {
    let file_name = format!("./{}", torrent.name);
    let piece_length = torrent.piece_length as usize;
    let total_length = torrent.length.unwrap_or(0) as usize;
    let num_pieces = torrent.pieces.len() / 20;

    // Open the file for writing.
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&file_name)
        .await
        .expect("Failed to open output file");

    // Preallocate the file size if total length is known.
    if total_length > 0 {
        file.set_len(total_length as u64).await.expect("Failed to set file length");
    }

    println!("Output actor: Writing to file '{}', total length: {} bytes, piece length: {} bytes, number of pieces: {}", 
             file_name, total_length, piece_length, num_pieces);

    while let Some(msg) = rx.recv().await {
        match msg {
            OutputMsg::Have((index, data)) => {
                if index >= num_pieces {
                    eprintln!("Output actor: Invalid piece index {}", index);
                    continue;
                }

                let offset = index * piece_length;
                let write_size = std::cmp::min(data.len(), piece_length);
                
                // Write the piece data at the correct offset.
                if let Err(e) = file.seek(std::io::SeekFrom::Start(offset as u64)).await {
                    eprintln!("Output actor: Failed to seek to offset {}: {}", offset, e);
                    continue;
                }

                if let Err(e) = file.write_all(&data).await {
                    eprintln!("Output actor: Failed to write piece {}: {}", index, e);
                    continue;
                }

                println!("Output actor: Wrote piece {} ({} bytes)", index, write_size);
            }
        }
    }

    println!("Output actor: Exiting");
}