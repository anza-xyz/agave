//! Transport tuning constants for a votor-style workload.
use {
    crate::{ALPENGLOW_ALPN, HANDSHAKE_DRAIN_RATE, MAX_INCOMING_DELAY},
    quinn::{
        ClientConfig, IdleTimeout, ServerConfig, TransportConfig, VarInt,
        crypto::rustls::{QuicClientConfig, QuicServerConfig},
    },
    solana_keypair::Keypair,
    solana_tls_utils::{
        new_dummy_x509_certificate, tls_client_config_builder, tls_server_config_builder,
    },
    std::{sync::Arc, time::Duration},
};
/// Close connections after this much time without feedback from peer.
/// This should be more than reasonable max time between PING (or data)
/// frames arriving from the peer. We also want to keep this rather short
/// to make sure we reinitiate connections that are truly stuck.
pub(crate) const MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
/// QUIC keep-alive heartbeat. Must be << `MAX_IDLE_TIMEOUT` to avoid
/// connections dying when no vote traffic is available.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(2);
/// MTU used for all datagrams. Path-MTU discovery is disabled, so the initial
/// and minimum MTU are the same; 1280 is the QUIC-spec floor.
const DATAGRAM_MTU: u16 = 1280;
/// Max number of connection attempts quinn buffers *per endpoint* before it starts
/// dropping Initials outright.
#[allow(clippy::arithmetic_side_effects)]
pub(crate) fn compute_max_incoming(num_endpoints: usize) -> usize {
    debug_assert!(num_endpoints > 0, "an endpoint needs at least one socket");
    let per_endpoint_rate = HANDSHAKE_DRAIN_RATE / num_endpoints;
    (per_endpoint_rate * MAX_INCOMING_DELAY.as_millis() as usize / 1000).max(1)
}

/// Creates a new transport config for datagram mode.
///
/// Datagram buffers are set to hold one second of traffic at `max_datagrams_per_second_per_peer`.
pub(crate) fn new_transport_config(max_datagrams_per_second_per_peer: usize) -> TransportConfig {
    let max_idle =
        IdleTimeout::try_from(MAX_IDLE_TIMEOUT).expect("MAX_IDLE_TIMEOUT fits IdleTimeout");
    let mut cfg = TransportConfig::default();
    // Ingress and egress get the same budget: one second of MTU-sized
    // datagrams at the max admitted rate.
    let data_buffer = max_datagrams_per_second_per_peer.saturating_mul(DATAGRAM_MTU as usize);
    cfg.datagram_receive_buffer_size(Some(data_buffer))
        .datagram_send_buffer_size(data_buffer)
        .initial_mtu(DATAGRAM_MTU)
        .min_mtu(DATAGRAM_MTU)
        .mtu_discovery_config(None)
        .keep_alive_interval(Some(KEEP_ALIVE_INTERVAL))
        .max_idle_timeout(Some(max_idle))
        // Datagrams only - disable streams entirely.
        .max_concurrent_bidi_streams(VarInt::from(0u8))
        .max_concurrent_uni_streams(VarInt::from(0u8));
    cfg
}

/// Build the rustls + quinn server config.
#[allow(clippy::arithmetic_side_effects)]
pub(crate) fn new_server_config(
    keypair: &Keypair,
    max_datagrams_per_second_per_peer: usize,
    num_endpoints: usize,
) -> ServerConfig {
    let (cert, key) = new_dummy_x509_certificate(keypair);
    let mut tls = tls_server_config_builder()
        .with_single_cert(vec![cert], key)
        .expect("rustls accepts our self-signed solana cert/key pair");
    tls.alpn_protocols = vec![ALPENGLOW_ALPN.to_vec()];
    tls.max_early_data_size = 0;
    let quic = QuicServerConfig::try_from(tls)
        .expect("TLS 1.3-only config yields an initial cipher suite");
    let mut cfg = ServerConfig::with_crypto(Arc::new(quic));
    // Do not buffer anything for a connection attempt that has not been accepted yet.
    cfg.incoming_buffer_size(0);
    cfg.incoming_buffer_size_total(0);
    cfg.max_incoming(compute_max_incoming(num_endpoints));
    cfg.retry_token_lifetime(MAX_IDLE_TIMEOUT);
    cfg.transport_config(Arc::new(new_transport_config(
        max_datagrams_per_second_per_peer,
    )));
    cfg.migration(false);
    cfg
}

/// Build the rustls + quinn client config.
pub(crate) fn new_client_config(
    keypair: &Keypair,
    max_datagrams_per_second_per_peer: usize,
) -> ClientConfig {
    let (cert, key) = new_dummy_x509_certificate(keypair);
    let mut tls = tls_client_config_builder()
        .with_client_auth_cert(vec![cert], key)
        .expect("rustls accepts our solana cert/key pair");
    tls.enable_early_data = false;
    tls.alpn_protocols = vec![ALPENGLOW_ALPN.to_vec()];
    let quic = QuicClientConfig::try_from(tls).expect("TLS config should be valid");
    let mut cfg = ClientConfig::new(Arc::new(quic));
    cfg.transport_config(Arc::new(new_transport_config(
        max_datagrams_per_second_per_peer,
    )));
    cfg
}

#[cfg(test)]
mod tests {
    use {
        super::compute_max_incoming,
        crate::{
            HANDSHAKE_DRAIN_RATE, HANDSHAKE_GLOBAL_RATE, HANDSHAKE_WORKERS_PER_ENDPOINT,
            MAX_ENDPOINTS, MAX_INCOMING_DELAY,
        },
    };

    #[test]
    fn max_incoming_splits_across_endpoints() {
        // One endpoint owns the entire drain budget for one full delay window.
        let total_buffered_handshakes = HANDSHAKE_DRAIN_RATE
            .saturating_mul(MAX_INCOMING_DELAY.as_millis() as usize)
            .saturating_div(1000);
        assert_eq!(
            compute_max_incoming(1),
            total_buffered_handshakes,
            "a lone endpoint should buffer MAX_INCOMING_DELAY worth of the HANDSHAKE_DRAIN_RATE \
             drain budget",
        );

        for num_endpoints in 1..=MAX_ENDPOINTS {
            let queue_length = compute_max_incoming(num_endpoints);

            let even_share = total_buffered_handshakes as f32 / num_endpoints as f32;
            assert!(
                (queue_length as f32 - even_share).abs() < 2.0,
                "each of {num_endpoints} should buffer ~{even_share} Initials, got {queue_length}",
            );
            assert!(even_share > 8.0, "sanity");

            let num_accept_loops = num_endpoints.saturating_mul(HANDSHAKE_WORKERS_PER_ENDPOINT);
            let per_loop_rate = HANDSHAKE_GLOBAL_RATE as f64 / num_accept_loops as f64;
            let per_endpoint_rate = per_loop_rate * HANDSHAKE_WORKERS_PER_ENDPOINT as f64;
            let delay_secs = queue_length as f64 / per_endpoint_rate;
            assert!(
                delay_secs <= MAX_INCOMING_DELAY.as_secs_f64(),
                "with {num_endpoints} endpoints a queued attempt waits up to {delay_secs}s, over \
                 the MAX_INCOMING_DELAY budget",
            );
        }
    }
}
