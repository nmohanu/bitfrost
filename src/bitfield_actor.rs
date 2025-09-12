use std::ops::Index;

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use crate::util::bitwise_and;

pub type BitfieldReceiver = mpsc::Receiver<BitfieldMsg>;
pub type BitfieldSender = mpsc::Sender<BitfieldMsg>;

pub const MAX_CONCURRENT_PEERS: usize = 30; // Limit concurrent peer connections.

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
    PieceFailed(Option<usize>),

    // Request allowance to download a piece.
    RequestPiece(usize, oneshot::Sender<bool>),

    // Check whether we are done.
    IsComplete(oneshot::Sender<bool>),
}

pub async fn bitfield_actor(mut rx: BitfieldReceiver, mut bitfield: Box<[bool]>) {
    let mut requested: Box<[bool]> = Box::from(vec![false; bitfield.len()]);
    let mut requestable: Box<[bool]> = bitfield.iter().map(|&x| !x).collect::<Vec<bool>>().into_boxed_slice();
    // Track how many times each piece is requested (for redundancy)
    let mut request_counts: Vec<usize> = vec![0; bitfield.len()];
    const MAX_REDUNDANT_REQUESTS: usize = 3; // Allow up to 3 workers per piece if needed

    while let Some(msg) = rx.recv().await {
        match msg {
            // Mark a piece as completed.
            BitfieldMsg::Have(index) => {
                if index < bitfield.len() {
                    bitfield[index] = true;
                    // Reset request count for completed piece
                    request_counts[index] = 0;
                }
                println!("Bitfield actor: Marked piece {} as completed", index);
                println!("Pieces done: {}/{}", bitfield.iter().filter(|&&x| x).count(), bitfield.len());
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
                    requestable[index] = false;
                }
            }
            // Return a requestable piece index, or None if no pieces available.
            BitfieldMsg::GetRequestablePiece(resp, peer_bitfield) => {
                let worker_can_request = bitwise_and(&peer_bitfield, &requestable);
                let available_count = worker_can_request.iter().filter(|&&x| x).count();
                println!("Bitfield actor: {} pieces available to request", available_count);

                // Try to find a never requested piece first.
                let mut next_piece = worker_can_request.iter().position(|&x| x);

                // If all pieces are already requested, allow multiple requests for unfinished pieces.
                if next_piece.is_none() {
                    next_piece = (0..bitfield.len())
                        .find(|&i| peer_bitfield[i] && !bitfield[i] && request_counts[i] < MAX_REDUNDANT_REQUESTS);
                }

                println!("Bitfield actor: next piece to request: {:?}", next_piece);
                let _ = resp.send(next_piece);
                // Mark the piece as requested and not requestable anymore.
                if let Some(idx) = next_piece {
                    request_counts[idx] += 1;
                    if !requested[idx] {
                        requested[idx] = true;
                        requestable[idx] = false;
                    }
                }
            }
            // Register a piece as failed.
            BitfieldMsg::PieceFailed(index) => {
                if let Some(idx) = index {
                    if idx < bitfield.len() {
                        // Mark the piece as not completed and requestable again.
                        requested[idx] = false;
                        requestable[idx] = true;
                        // Reset request count so it can be redundantly requested again.
                        request_counts[idx] -= 1;
                    }
                }
            }
            // Request allowance to download a piece.
            BitfieldMsg::RequestPiece(index, resp) => {
                let can_request = if index < bitfield.len() {
                    !bitfield[index] && !requested[index]
                } else {
                    false
                };
                if can_request {
                    requested[index] = true;
                    requestable[index] = false;
                }
                let _ = resp.send(can_request);
            }
            // Check whether all pieces are completed.
            BitfieldMsg::IsComplete(resp) => {
                let complete = bitfield.iter().all(|&x| x);
                let _ = resp.send(complete);
            }
        }
    }
}

pub async fn get_requestable_piece(tx: &BitfieldSender, peer_bitfield: Box<[bool]>) -> Result<Option<usize>, Error> {
    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(BitfieldMsg::GetRequestablePiece(resp_tx, peer_bitfield)).await?;
    let piece = resp_rx.await?;
    Ok(piece)
}

#[derive(Error, Debug)]
pub enum Error {
    #[error(transparent)]
    SendError(#[from] tokio::sync::mpsc::error::SendError<BitfieldMsg>),

    #[error(transparent)]
    ReceiveError(#[from] tokio::sync::oneshot::error::RecvError),
}

