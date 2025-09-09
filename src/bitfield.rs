use std::ops::Index;

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use crate::util::bitwise_and;

pub type Receiver = mpsc::Receiver<BitfieldMsg>;
pub type Sender = mpsc::Sender<BitfieldMsg>;

#[derive(Debug)]
pub enum BitfieldMsg {
    /// Mark a piece as completed
    Have(usize),

    /// Query the whole bitfield (snapshot)
    Get(oneshot::Sender<Box<[bool]>>),

    /// Check if a piece is available
    HasPiece(usize, oneshot::Sender<bool>),

    // Register a piece as requested (not yet implemented).
    Requested(usize),

    // Query the requested pieces bitfield.
    GetRequestablePiece(oneshot::Sender<Option<usize>>, Box<[bool]>),

    // Register a piece as failed.
    PieceFailed(u32),
}

pub async fn bitfield_actor(mut rx: Receiver, mut bitfield: Box<[bool]>) {
    let mut requested: Box<[bool]> = Box::from(vec![false; bitfield.len()]);
    let mut requestable: Box<[bool]> = bitfield.iter().map(|&x| !x).collect::<Vec<bool>>().into_boxed_slice();

    while let Some(msg) = rx.recv().await {
        match msg {
            // Mark a piece as completed.
            BitfieldMsg::Have(index) => {
                if index < bitfield.len() {
                    bitfield[index] = true;
                }
            }
            // Return a snapshot of the current bitfield.
            BitfieldMsg::Get(resp) => {
                let snapshot = bitfield.clone();
                let _ = resp.send(snapshot);
            }
            // Check if a specific piece is available.
            BitfieldMsg::HasPiece(idx, resp) => {
                let has = bitfield.get(idx).copied().unwrap_or(false);
                let _ = resp.send(has);
            }
            // Register a piece as requested.
            BitfieldMsg::Requested(index) => {
                if index < bitfield.len() {
                    requested[index] = true;
                }
            }
            // Return a requestable piece index, or None if no pieces available.
            BitfieldMsg::GetRequestablePiece(resp, peer_bitfield) => {
                let worker_can_request = bitwise_and(&peer_bitfield, &requestable);
                let available_count = worker_can_request.iter().filter(|&&x| x).count();
                println!("Bitfield actor: {} pieces available to request", available_count);
                
                let next_piece = worker_can_request.iter().position(|&x| x);
                println!("Bitfield actor: next piece to request: {:?}", next_piece);
                
                let _ = resp.send(next_piece);
                // Mark the piece as requested and not requestable anymore.
                if let Some(idx) = next_piece {
                    requested[idx] = true;
                    requestable[idx] = false;
                }
            }
            // Register a piece as failed.
            BitfieldMsg::PieceFailed(index) => {
                if (index as usize) < bitfield.len() {
                    // Mark the piece as not completed and requestable again.
                    requested[index as usize] = false;
                    requestable[index as usize] = true;
                }
            }
        }
    }
}

pub async fn get_requestable_piece(tx: &Sender, peer_bitfield: Box<[bool]>) -> Result<Option<usize>, Error> {
    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(BitfieldMsg::GetRequestablePiece(resp_tx, peer_bitfield)).await?;
    let piece = resp_rx.await?;
    Ok(piece)
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("Error with getting bitfield snapshot: {0}")]
    SnapshotError(String),  

    #[error(transparent)]
    SendError(#[from] tokio::sync::mpsc::error::SendError<BitfieldMsg>),

    #[error(transparent)]
    ReceiveError(#[from] tokio::sync::oneshot::error::RecvError),
}

