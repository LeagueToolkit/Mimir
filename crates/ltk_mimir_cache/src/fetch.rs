//! Bundled, feature-gated [`Fetch`](crate::Fetch)/[`AsyncFetch`](crate::AsyncFetch)
//! implementations for the common case: pulling `.lhdb` release assets over
//! HTTP.
//!
//! The core crate ships no HTTP client - [`HashStore::update`](crate::HashStore::update)
//! takes a caller-supplied fetcher. These optional types remove the
//! release-asset glue that every consumer would otherwise rewrite: a
//! [`ReleaseSource`] (a GitHub latest-release layout or an explicit mirror base
//! URL) plus a concrete [`HttpFetchError`], so no consumer writes the error
//! plumbing the [`Fetch::Error`](crate::Fetch::Error) `Sized` bound would
//! otherwise force.
//!
//! - `ureq` feature → [`UreqFetch`], a blocking [`Fetch`](crate::Fetch).
//! - `reqwest` feature → [`ReqwestFetch`], an async
//!   [`AsyncFetch`](crate::AsyncFetch).
//!
//! Both fetchers stream into the caller's sink rather than buffering a whole
//! table, and both are silent by design: per-file progress and cancellation stay
//! with the caller, who wraps the fetcher and passes a sink of their own (see
//! [`Fetch`](crate::Fetch)).

/// Default `User-Agent` for the bundled fetchers, matching the mimir CLI.
const USER_AGENT: &str = concat!("mimir/", env!("CARGO_PKG_VERSION"));

/// Where release assets live: a GitHub repo's latest release, or an explicit
/// base URL (a mirror). Shared by the blocking and async fetchers.
#[derive(Debug, Clone)]
pub struct ReleaseSource {
    base: String,
}

impl ReleaseSource {
    /// The GitHub latest-release layout:
    /// `https://github.com/{owner_repo}/releases/latest/download`.
    pub fn github(owner_repo: &str) -> Self {
        Self {
            base: format!("https://github.com/{owner_repo}/releases/latest/download"),
        }
    }

    /// An explicit base URL serving `manifest.json` + the `.lhdb` assets (a
    /// mirror). A trailing slash is trimmed so asset URLs join cleanly.
    pub fn base_url(url: impl Into<String>) -> Self {
        let url = url.into();
        Self {
            base: url.trim_end_matches('/').to_owned(),
        }
    }

    /// The full URL for one asset filename under this source.
    fn asset_url(&self, filename: &str) -> String {
        format!("{}/{filename}", self.base)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HttpFetchError {
    /// The request never produced an HTTP response (DNS, connect, TLS, or a
    /// mid-body read failure).
    #[error("fetching {url}")]
    Transport {
        url: String,

        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The server answered, but with a non-success status.
    #[error("unexpected HTTP {status} for {url}")]
    Status { status: u16, url: String },
}

/// The read buffer both fetchers pump through. Large enough that a 38 MiB table
/// is a few hundred round trips, small enough to stay off the radar.
#[cfg(any(feature = "ureq", feature = "reqwest"))]
const CHUNK: usize = 64 * 1024;

#[cfg(feature = "ureq")]
mod ureq_impl {
    use std::io::{Read, Write};

    use super::{HttpFetchError, ReleaseSource, CHUNK, USER_AGENT};
    use crate::{Fetch, FetchError};

    /// A blocking [`Fetch`] over `ureq`, pulling assets from a [`ReleaseSource`].
    pub struct UreqFetch {
        source: ReleaseSource,

        agent: ureq::Agent,
    }

    impl UreqFetch {
        /// A fetcher for `source`, using a fresh agent with the mimir
        /// `User-Agent`.
        pub fn new(source: ReleaseSource) -> Self {
            let agent = ureq::AgentBuilder::new().user_agent(USER_AGENT).build();

            Self { source, agent }
        }
    }

    impl Fetch for UreqFetch {
        type Error = HttpFetchError;

        fn fetch_to(
            &self,
            filename: &str,
            sink: &mut (dyn Write + Send),
        ) -> Result<u64, FetchError<HttpFetchError>> {
            let url = self.source.asset_url(filename);

            // `call()` fails on non-2xx, so a 404 arrives as `Error::Status`.
            let response = match self.agent.get(&url).call() {
                Ok(response) => response,
                Err(ureq::Error::Status(status, _)) => {
                    return Err(FetchError::Transport(HttpFetchError::Status {
                        status,
                        url,
                    }))
                }
                Err(err) => {
                    return Err(FetchError::Transport(HttpFetchError::Transport {
                        url,
                        source: Box::new(err),
                    }))
                }
            };

            // Hand-rolled rather than `io::copy`, which would fold a mid-body
            // read failure and the caller's sink refusing a chunk into one error.
            let mut reader = response.into_reader();
            let mut buf = vec![0u8; CHUNK];
            let mut total = 0;
            loop {
                let read = reader.read(&mut buf).map_err(|err| {
                    FetchError::Transport(HttpFetchError::Transport {
                        url: url.clone(),
                        source: Box::new(err),
                    })
                })?;
                if read == 0 {
                    return Ok(total);
                }

                sink.write_all(&buf[..read]).map_err(FetchError::Sink)?;
                total += read as u64;
            }
        }
    }
}

#[cfg(feature = "ureq")]
pub use ureq_impl::UreqFetch;

#[cfg(feature = "reqwest")]
mod reqwest_impl {
    use std::future::Future;
    use std::io::Write;

    use super::{HttpFetchError, ReleaseSource, USER_AGENT};
    use crate::{AsyncFetch, FetchError};

    pub struct ReqwestFetch {
        source: ReleaseSource,

        client: reqwest::Client,
    }

    impl ReqwestFetch {
        /// A fetcher for `source`, using a client with the mimir `User-Agent`.
        pub fn new(source: ReleaseSource) -> Self {
            let client = reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .build()
                .unwrap_or_default();

            Self { source, client }
        }
    }

    impl AsyncFetch for ReqwestFetch {
        type Error = HttpFetchError;

        fn fetch_to<'a>(
            &'a self,
            filename: &'a str,
            sink: &'a mut (dyn Write + Send),
        ) -> impl Future<Output = Result<u64, FetchError<HttpFetchError>>> + Send + 'a {
            // Own what the request needs; only the sink is borrowed.
            let url = self.source.asset_url(filename);
            let client = self.client.clone();

            async move {
                let transport = |err: reqwest::Error, url: &str| {
                    FetchError::Transport(HttpFetchError::Transport {
                        url: url.to_owned(),
                        source: Box::new(err),
                    })
                };

                let mut response = client
                    .get(&url)
                    .send()
                    .await
                    .map_err(|err| transport(err, &url))?;

                let status = response.status();
                if !status.is_success() {
                    return Err(FetchError::Transport(HttpFetchError::Status {
                        status: status.as_u16(),
                        url,
                    }));
                }

                // `chunk` rather than `bytes`, so the body never has to exist in
                // memory all at once. It also keeps the transport failure and the
                // sink failure apart.
                let mut total = 0;
                while let Some(chunk) =
                    response.chunk().await.map_err(|err| transport(err, &url))?
                {
                    sink.write_all(&chunk).map_err(FetchError::Sink)?;
                    total += chunk.len() as u64;
                }

                Ok(total)
            }
        }
    }
}

#[cfg(feature = "reqwest")]
pub use reqwest_impl::ReqwestFetch;

#[cfg(all(test, any(feature = "ureq", feature = "reqwest")))]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread::{self, JoinHandle};

    use super::*;
    #[cfg(feature = "reqwest")]
    use crate::AsyncFetch;
    #[cfg(feature = "ureq")]
    use crate::Fetch;
    use crate::FetchError;

    /// A throwaway HTTP/1.1 server: serves `payload` at `ok_path`, 404s
    /// everything else, and handles exactly `connections` requests (one per
    /// connection) before the thread exits.
    fn serve(payload: Vec<u8>, ok_path: String, connections: usize) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());

        let handle = thread::spawn(move || {
            for _ in 0..connections {
                let (mut stream, _) = listener.accept().unwrap();
                handle_one(&mut stream, &payload, &ok_path);
            }
        });

        (base, handle)
    }

    /// Route a single request off its request line and write one response.
    fn handle_one(stream: &mut TcpStream, payload: &[u8], ok_path: &str) {
        let mut buf = [0u8; 1024];
        let n = stream.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]);
        let path = request.split_whitespace().nth(1).unwrap_or("");

        if path == ok_path {
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                payload.len()
            );
            stream.write_all(header.as_bytes()).unwrap();
            stream.write_all(payload).unwrap();
        } else {
            stream
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        }

        stream.flush().unwrap();
    }

    #[cfg(feature = "ureq")]
    #[test]
    fn ureq_fetch_returns_bytes_and_maps_404() {
        let payload = b"lhdb-bytes".to_vec();
        let (base, server) = serve(payload.clone(), "/game-1.lhdb".to_string(), 2);

        // Two fetchers → two connections, matching the server's count.
        // Straight into a caller's sink - no whole-table buffer anywhere.
        let ok = UreqFetch::new(ReleaseSource::base_url(&base));
        let mut sink = Vec::new();
        assert_eq!(
            ok.fetch_to("game-1.lhdb", &mut sink).unwrap(),
            payload.len() as u64
        );
        assert_eq!(sink, payload);

        let missing = UreqFetch::new(ReleaseSource::base_url(&base));
        match missing.fetch("nope.lhdb") {
            Err(FetchError::Transport(HttpFetchError::Status { status: 404, .. })) => {}
            other => panic!("expected a 404 status error, got {other:?}"),
        }

        server.join().unwrap();
    }

    #[cfg(feature = "reqwest")]
    #[test]
    fn reqwest_fetch_returns_bytes_and_maps_404() {
        let payload = b"lhdb-bytes".to_vec();
        let (base, server) = serve(payload.clone(), "/game-1.lhdb".to_string(), 2);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let ok = ReqwestFetch::new(ReleaseSource::base_url(&base));
            let mut sink = Vec::new();
            assert_eq!(
                ok.fetch_to("game-1.lhdb", &mut sink).await.unwrap(),
                payload.len() as u64
            );
            assert_eq!(sink, payload);

            let missing = ReqwestFetch::new(ReleaseSource::base_url(&base));
            match missing.fetch("nope.lhdb").await {
                Err(FetchError::Transport(HttpFetchError::Status { status: 404, .. })) => {}
                other => panic!("expected a 404 status error, got {other:?}"),
            }
        });

        server.join().unwrap();
    }
}
