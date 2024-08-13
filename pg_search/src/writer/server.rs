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

use super::{Handler, IndexError, ServerRequest, WriterDirectory, WriterRequest};
use crate::writer::{transfer, Writer};
use rayon::ThreadPool;
use std::collections::HashMap;
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use std::{io, sync::Mutex};
use tantivy::IndexWriter;
use thiserror::Error;
use tracing::{debug, error};

/// A generic server for receiving requests and transfers from a client.
pub struct Server {
    addr: std::net::SocketAddr,
    http: tiny_http::Server,
    thread_pool: ThreadPool,
    tantivy_writers: Arc<Mutex<HashMap<WriterDirectory, IndexWriter>>>,
}

impl Server {
    pub fn new() -> Result<Self, ServerError> {
        let http = tiny_http::Server::http("0.0.0.0:0")
            .map_err(|err| ServerError::AddressBindFailed(err.to_string()))?;

        let addr = match http.server_addr() {
            tiny_http::ListenAddr::IP(addr) => addr,
            tiny_http::ListenAddr::Unix(addr) => {
                return Err(ServerError::UnixSocketBindAttempt(format!("{addr:?}")))
            }
        };

        let thread_pool = rayon::ThreadPoolBuilder::new().build().unwrap();

        let tantivy_writers = Arc::new(Mutex::new(HashMap::new()));

        Ok(Self {
            addr,
            http,
            thread_pool, // Initialize the thread pool
            tantivy_writers,
        })
    }

    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }

    pub fn start(&mut self) -> Result<(), ServerError> {
        self.listen_request()
    }

    fn listen_transfer<P: AsRef<Path>>(
        pipe_path: P,
        tantivy_writers: Arc<Mutex<HashMap<WriterDirectory, IndexWriter>>>,
    ) -> Result<(), ServerError> {
        // Our consumer will receive messages suitable for our handler.
        for incoming in transfer::read_stream::<WriterRequest, P>(pipe_path)? {
            let mut handler = Writer::new(tantivy_writers.clone());
            handler.handle(incoming?).map_err(ServerError::Anyhow)?;
        }
        Ok(())
    }

    fn response_ok() -> tiny_http::Response<io::Empty> {
        tiny_http::Response::empty(200)
    }

    fn response_err(err: ServerError) -> tiny_http::Response<Cursor<Vec<u8>>> {
        tiny_http::Response::from_string(err.to_string()).with_status_code(500)
    }

    fn listen_request(&mut self) -> Result<(), ServerError> {
        debug!(address = %self.addr, "listening to incoming requests");

        self.thread_pool.install(|| {
            for mut incoming in self.http.incoming_requests() {
                let tantivy_writers = Arc::clone(&self.tantivy_writers);

                self.thread_pool.spawn(move || {
                    let reader = incoming.as_reader();
                    let request: Result<ServerRequest<WriterRequest>, ServerError> =
                        bincode::deserialize_from(reader)
                            .map_err(|err| ServerError::Unexpected(err.into()));

                    match request {
                        Ok(req) => match req {
                            ServerRequest::Shutdown => {
                                if let Err(err) = incoming.respond(Self::response_ok()) {
                                    error!("server error responding to shutdown: {err}");
                                }
                            }
                            ServerRequest::Transfer(pipe_path) => {
                                if let Err(err) = incoming.respond(Self::response_ok()) {
                                    error!("server error responding to transfer: {err}");
                                } else if let Err(err) =
                                    Self::listen_transfer(pipe_path, tantivy_writers)
                                {
                                    error!("error listening to transfer: {err}")
                                }
                            }
                            ServerRequest::Request(req) => {
                                let mut handler = Writer::new(tantivy_writers.clone());
                                if let Err(err) = handler.handle(req) {
                                    if let Err(err) = incoming
                                        .respond(Self::response_err(ServerError::Anyhow(err)))
                                    {
                                        error!("server error responding to handler error: {err}");
                                    }
                                } else if let Err(err) = incoming.respond(Self::response_ok()) {
                                    error!("server error responding to handler success: {err}")
                                }
                            }
                        },
                        Err(err) => {
                            if let Err(err) = incoming.respond(Self::response_err(err)) {
                                error!(
                                    "server error responding to client on deserialize error: {err}"
                                );
                            }
                        }
                    };
                });
            }
        });

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
