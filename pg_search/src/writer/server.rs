// Copyright (c) 2023-2024 Retake, Inc.
//
// This file is part of ParadeDB - Postgres for Search and Analytics
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use super::{Handler, IndexError, ServerRequest, TransactionState};
use crate::writer::transfer;
use anyhow::{anyhow, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::HashMap;
use std::io;
use std::marker::PhantomData;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
use thiserror::Error;
use tracing::{debug, error, trace};

pub const TRANSACTION_ID_HEADER: &str = "TransactionID";

type IndexOID = u32;
type TransactionID = u64;

/// A generic server for receiving requests and transfers from a client.
pub struct Server<'a, T, H>
where
    T: DeserializeOwned,
    H: Handler<T>,
{
    addr: std::net::SocketAddr,
    http: tiny_http::Server,
    marker: PhantomData<&'a T>,
    handler_marker: PhantomData<&'a H>,
    channels: Arc<RwLock<HashMap<u64, Sender<tiny_http::Request>>>>,
    shutdown_flag: Arc<AtomicBool>,
}

impl<'a, T, H> Server<'a, T, H>
where
    T: Serialize + DeserializeOwned + 'a,
    H: Handler<T>,
{
    pub fn new() -> Result<Self, ServerError> {
        let http = tiny_http::Server::http("0.0.0.0:0")
            .map_err(|err| ServerError::AddressBindFailed(err.to_string()))?;

        let addr = match http.server_addr() {
            tiny_http::ListenAddr::IP(addr) => addr,
            // It's not clear when tiny_http would choose to use a Unix socket address,
            // but we have to handle the enum variant, so we'll consider this outcome
            // an irrecovereable error, although its not expected to happen.
            tiny_http::ListenAddr::Unix(addr) => {
                return Err(ServerError::UnixSocketBindAttempt(format!("{addr:?}")))
            }
        };

        Ok(Self {
            addr,
            http,
            marker: PhantomData,
            handler_marker: PhantomData,
            channels: Arc::new(RwLock::new(HashMap::new())),
            index_locks: Arc::new(HashMap::new()),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }

    pub fn start(&mut self) -> Result<(), ServerError> {
        self.listen()
    }

    fn listen_transfer<P: AsRef<Path>>(
        shutdown_flag: Arc<AtomicBool>,
        pipe_path: P,
        handler: &mut H,
    ) -> Result<TransactionState, ServerError> {
        // Our consumer will receive messages suitable for our handler.
        for incoming in transfer::read_stream::<T, P>(pipe_path)? {
            // Another thread has triggered a shutdown. We must quit.
            if shutdown_flag.load(Ordering::SeqCst) {
                return Err(ServerError::from(anyhow!(
                    "writer server transaction thread received shutdown during insert"
                )));
            }

            let state = handler.handle(incoming?).map_err(ServerError::Anyhow)?;

            // Check if the handler has indicated to complete the transaction, and if
            // so return early. This is not expected, as a pipe transfer should only
            // be used for insert requests, not commit requests.
            if matches!(state, TransactionState::Complete) {
                return Ok(TransactionState::Complete);
            }
        }
        Ok(TransactionState::Continue)
    }

    fn response_ok() -> tiny_http::Response<io::Empty> {
        tiny_http::Response::empty(200)
    }

    fn respond_err(request: tiny_http::Request, err: ServerError) {
        let response = tiny_http::Response::from_string(err.to_string()).with_status_code(500);
        if let Err(respond_err) = request.respond(response) {
            error!(?respond_err, ?err, "failed to respond to client with error");
        }
    }

    fn transaction_id(request: &tiny_http::Request) -> Option<&str> {
        request
            .headers()
            .iter()
            // The reqwest library, used by the client, lowercases all header keys.
            .find(|header| header.field.as_str().as_str() == TRANSACTION_ID_HEADER.to_lowercase())
            .map(|header| header.value.as_str())
            .and_then(|header| {
                // Consider an empty header as a missing header.
                if header.is_empty() {
                    None
                } else {
                    Some(header)
                }
            })
    }

    fn handle_request(
        transaction_id: TransactionID,
        shutdown_flag: Arc<AtomicBool>,
        mut incoming: tiny_http::Request,
        handler: &mut H,
    ) {
        // Deserialize the value into a ServerRequest.
        let reader = incoming.as_reader();
        let request: Result<ServerRequest<T>, ServerError> =
            bincode::deserialize_from(reader).map_err(|err| ServerError::Unexpected(err.into()));

        match request {
            Ok(req) => match req {
                ServerRequest::Shutdown => {
                    // We've received a shutdown request, we must flip the shutdown_flag
                    // to notify other threads. This thread will exit on the next loop.
                    trace!(
                        transaction_id,
                        "writer server thread setting shutdown flag to true"
                    );
                    shutdown_flag.swap(true, Ordering::SeqCst);

                    if let Err(err) = incoming.respond(Self::response_ok()) {
                        error!(transaction_id, ?err, "server error responding to shutdown");
                    }
                }
                ServerRequest::Transfer(pipe_path) => {
                    // We must respond with OK before initiating the transfer.
                    if let Err(err) = incoming.respond(Self::response_ok()) {
                        return error!(
                            transaction_id,
                            "server error responding to transfer: {err}"
                        );
                    } else {
                        match Self::listen_transfer(shutdown_flag.clone(), pipe_path, handler) {
                            Ok(transaction_state) => {
                                // If the transaction is complete, we should exit the thread.
                                if matches!(transaction_state, TransactionState::Complete) {
                                    return;
                                }
                            }

                            Err(err) => {
                                return error!(transaction_id, ?err, "error listening to transfer");
                            }
                        }
                    }
                }
                ServerRequest::Request(req) => match handler.handle(req) {
                    Ok(transaction_state) => {
                        // If we cannot respond to the client, we must exit the thread.
                        if let Err(err) = incoming.respond(Self::response_ok()) {
                            return error!(
                                transaction_id,
                                ?err,
                                "error in writer server responding to handler success"
                            );
                        }
                        // If the handler indicates that the transaction is complete,
                        // we must exit the thread.
                        if matches!(transaction_state, TransactionState::Complete) {
                            return;
                        }
                    }
                    Err(err) => {
                        error!(
                            transaction_id,
                            ?err,
                            "error in writer server request handler"
                        );
                        return Self::respond_err(incoming, ServerError::Anyhow(err));
                    }
                },
            },

            Err(err) => {
                error!(
                    transaction_id,
                    ?err,
                    "error deserializing writer server request"
                );
                return Self::respond_err(incoming, err);
            }
        };
    }

    fn listen_transaction(
        transaction_id: TransactionID,
        shutdown_flag: Arc<AtomicBool>,
        receiver: Receiver<tiny_http::Request>,
    ) {
        // Initialize a handler for this transaction.
        let mut handler: H = Default::default();

        trace!(
            transaction_id,
            "writer server starting transaction thread loop"
        );
        // The main "listen" loop, which keeps the thread alive for the life of a client transaction.
        // We will return from the loop at any time if there is any error.
        loop {
            if shutdown_flag.load(Ordering::SeqCst) {
                trace!(transaction_id, "writer server thread shutting down");
                // Another thread has triggered a shutdown. We must quit.
                return;
            }

            // Wait for a value to come through the receiver.
            trace!(
                transaction_id,
                "writer server thread awaiting incoming message through channel"
            );
            let incoming = match receiver.recv() {
                Ok(request) => request,
                Err(err) => {
                    return error!(
                        ?err,
                        "unexpected error receving request in writer server transaction"
                    );
                }
            };
            trace!(
                transaction_id,
                "writer server thread received incoming message through channel"
            );

            Self::handle_request(
                transaction_id,
                shutdown_flag.clone(),
                incoming,
                &mut handler,
            )
        }
    }
    fn dispatch_transaction_thread(
        &self,
        transaction_id: TransactionID,
        request: tiny_http::Request,
    ) {
        // Get a write lock on the channels map.
        let channels_lock = self.channels.clone();
        let mut channels = match channels_lock.write() {
            Ok(channels) => channels,
            Err(err) => {
                let error = "unexpected error sending request data to server transaction thread";
                error!(?err, "{error}");
                return Self::respond_err(request, ServerError::from(anyhow!("{error}: {err}")));
            }
        };

        trace!(
            transaction_id,
            "writer server acquired channels map lock, initializing sender"
        );

        // If the sender already exists, it means there is a transaction already in progress.
        // Retrieve the sender channel from the channels map so we can send the new request.
        // If no sender exists, it means this is the first request in the transaction, so
        // we should start a new thread to and cache its sender channel.
        let sender = channels.entry(transaction_id).or_insert_with(|| {
            let (sender, receiver) = std::sync::mpsc::channel();
            let shutdown_flag = self.shutdown_flag.clone();
            let channels_lock = self.channels.clone();

            trace!(
                transaction_id,
                "writer server spawning new transaction thread"
            );
            std::thread::spawn(move || {
                trace!(transaction_id, "writer server thread spawned");
                // Starting listening for an incoming request for this transaction.
                Self::listen_transaction(transaction_id, shutdown_flag, receiver);

                // Need to get a write lock on the channels map from within the
                // thread so that we can clean up the Transacton: Sender entry.
                match channels_lock.write() {
                    Ok(mut channels) => {
                        channels.remove(&transaction_id);
                    }
                    Err(err) => {
                        error!(
                            ?err,
                            transaction_id, "writer server failed to clean up thread channels"
                        );
                    }
                };
            });
            sender
        });

        trace!(
            transaction_id,
            "writer server sending request to transaction thread"
        );
        if let Err(err) = sender.send(request) {
            let request = err.0; // Recover request from error.
            let error = "unexpected error sending request data to server transaction thread";
            error!("{error}");
            Self::respond_err(request, ServerError::from(anyhow!("{error}")));
        }
    }

    fn listen(&mut self) -> Result<(), ServerError> {
        debug!(address = %self.addr, "listening to incoming requests");

        for incoming in self.http.incoming_requests() {
            trace!(
                headers = ?incoming.headers(),
                "writer server received request"
            );

            match Self::transaction_id(&incoming) {
                // If there's a TransactionID header, parse the ID and dispatch the
                // request to a dedicated thread for the transaction.
                Some(id_string) => match id_string.parse() {
                    Ok(transaction_id) => {
                        trace!(transaction_id, "writer server dispatching incoming request");
                        self.dispatch_transaction_thread(transaction_id, incoming);
                    }
                    Err(err) => {
                        let parse_err =
                            format!("error parsing '{TRANSACTION_ID_HEADER}' into TransactionID");
                        error!(?err, message = parse_err);
                        Self::respond_err(incoming, ServerError::Anyhow(anyhow!("{parse_err}")));
                    }
                },
                // If there is no TransactionID, handle the request here in the main thread.
                None => {
                    let mut handler: H = Default::default();
                    Self::handle_request(0, self.shutdown_flag.clone(), incoming, &mut handler);
                }
            };

            // In case we received a shutdown request above, we should check if the
            // shutdown_flag has been set. If so, we should quit the server.
            if self.shutdown_flag.load(Ordering::SeqCst) {
                debug!("writer server main thread received shutdown request");
                return Ok(());
            }
        }

        unreachable!("server should never stop listening");
    }
}

#[derive(Error, Debug)]
pub enum ServerError {
    #[error("couldn't open the consumer pipe file: {0}")]
    OpenPipeFile(std::io::Error),

    #[error("error binding writer server to address: {0}")]
    AddressBindFailed(String),

    #[error("writer server must not bind to unix socket, attemped: {0}")]
    UnixSocketBindAttempt(String),

    #[error(transparent)]
    WriterError(#[from] IndexError),

    #[error(transparent)]
    IOError(#[from] std::io::Error),

    #[error(transparent)]
    ReqwestError(#[from] reqwest::Error),

    #[error(transparent)]
    SerdeJson(#[from] serde_json::Error),

    #[error(transparent)]
    Bincode(#[from] bincode::Error),

    #[error("unexpected error: {0}")]
    Unexpected(#[from] Box<dyn std::error::Error>),

    #[error("unexpected error: {0}")]
    Anyhow(#[from] anyhow::Error),
}

#[cfg(test)]
mod tests {
    use crate::{
        fixtures::*,
        schema::{SearchDocument, SearchIndexSchema},
    };
    use anyhow::Result;
    use rstest::*;
    use tantivy::Index;

    #[rstest]
    fn test_index_commit(
        simple_schema: SearchIndexSchema,
        simple_doc: SearchDocument,
        mock_dir: MockWriterDirectory,
    ) -> Result<()> {
        let tantivy_path = mock_dir.tantivy_dir_path(true)?;
        let index = Index::builder()
            .schema(simple_schema.into())
            .create_in_dir(tantivy_path)
            .unwrap();

        let mut writer: tantivy::IndexWriter<tantivy::TantivyDocument> =
            index.writer(500_000_000).unwrap();
        writer.add_document(simple_doc.into()).unwrap();
        writer.commit().unwrap();

        Ok(())
    }
}
