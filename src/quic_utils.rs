use quinn::crypto::rustls::QuicClientConfig;
use quinn::{
    ClientConfig, Endpoint, EndpointConfig, IdleTimeout, TransportConfig, default_runtime,
};
use solana_tls_utils::{QuicClientCertificate, tls_client_config_builder};
use solana_tpu_client_next::connection_workers_scheduler::BindTarget;
use std::sync::Arc;
use std::time::Duration;

pub const ALPN_TPU_PROTOCOL_ID: &[u8] = b"solana-tpu";
pub const QUIC_MAX_TIMEOUT: Duration = Duration::from_secs(30);
pub const QUIC_KEEP_ALIVE: Duration = Duration::from_secs(1);

use {
    quinn::{ConnectError, ConnectionError, WriteError},
    std::{
        fmt::{self, Formatter},
        io,
    },
    thiserror::Error,
};

/// Wrapper for [`io::Error`] implementing [`PartialEq`] to simplify error
/// checking for the [`QuicError`] type. The reasons why [`io::Error`] doesn't
/// implement [`PartialEq`] are discusses in
/// <https://github.com/rust-lang/rust/issues/34158>.
#[derive(Debug, Error)]
pub struct IoErrorWithPartialEq(pub io::Error);

impl PartialEq for IoErrorWithPartialEq {
    fn eq(&self, other: &Self) -> bool {
        let formatted_self = format!("{self:?}");
        let formatted_other = format!("{other:?}");
        formatted_self == formatted_other
    }
}

impl fmt::Display for IoErrorWithPartialEq {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<io::Error> for IoErrorWithPartialEq {
    fn from(err: io::Error) -> Self {
        IoErrorWithPartialEq(err)
    }
}

/// Error types that can occur when dealing with QUIC connections or
/// transmissions.
#[derive(Error, Debug, PartialEq)]
pub enum QuicError {
    #[error(transparent)]
    StreamWrite(#[from] WriteError),
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    #[error(transparent)]
    Connect(#[from] ConnectError),
    #[error(transparent)]
    Endpoint(#[from] IoErrorWithPartialEq),
    #[error("Handshake timeout")]
    HandshakeTimeout,
}

pub fn create_client_config(client_certificate: &QuicClientCertificate) -> ClientConfig {
    let mut crypto = tls_client_config_builder()
        .with_client_auth_cert(
            vec![client_certificate.certificate.clone()],
            client_certificate.key.clone_key(),
        )
        .expect("Failed to set QUIC client certificates");
    crypto.enable_early_data = true;
    crypto.alpn_protocols = vec![ALPN_TPU_PROTOCOL_ID.to_vec()];

    let transport_config = {
        let mut res = TransportConfig::default();

        let timeout = IdleTimeout::try_from(QUIC_MAX_TIMEOUT).unwrap();
        res.max_idle_timeout(Some(timeout));
        res.keep_alive_interval(Some(QUIC_KEEP_ALIVE));
        // Disable Quic send fairness.
        // When set to false, streams are still scheduled based on priority,
        // but once a chunk of a stream has been written out, quinn tries to complete
        // the stream instead of trying to round-robin balance it among the streams
        // with the same priority.
        // See https://github.com/quinn-rs/quinn/pull/2002.
        res.send_fairness(false);

        res
    };

    let mut config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(crypto).unwrap()));
    config.transport_config(Arc::new(transport_config));

    config
}

pub fn create_client_endpoint(
    bind: BindTarget,
    client_config: ClientConfig,
) -> Result<Endpoint, QuicError> {
    let mut endpoint = match bind {
        BindTarget::Address(bind_addr) => {
            Endpoint::client(bind_addr).map_err(IoErrorWithPartialEq::from)?
        }
        BindTarget::Socket(socket) => {
            let runtime = default_runtime()
                .ok_or_else(|| std::io::Error::other("no async runtime found"))
                .map_err(IoErrorWithPartialEq::from)?;
            Endpoint::new(EndpointConfig::default(), None, socket, runtime)
                .map_err(IoErrorWithPartialEq::from)?
        }
    };
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}
