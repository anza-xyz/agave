//! This module defines [`SendTransactionStats`] which is used to collect per IP
//! statistics about relevant network errors.
use {
    super::QuicError,
    quinn::{ConnectError, ConnectionError, WriteError},
    std::{collections::HashMap, fmt, net::IpAddr},
};

/// [`SendTransactionStats`] aggregates counters related to sending transactions.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SendTransactionStats {
    pub successfully_sent: u64,
    pub connect_error_invalid_remote_address: u64,
    pub connect_error_other: u64,
    pub connect_error_too_many_connections: u64,
    pub connection_error_application_closed: u64,
    pub connection_error_connection_closed: u64,
    pub connection_error_locally_closed: u64,
    pub connection_error_reset: u64,
    pub connection_error_timed_out: u64,
    pub connection_error_transport_error: u64,
    pub connection_error_version_mismatch: u64,
    pub write_error_connection_lost: u64,
    pub write_error_stopped: u64,
    pub write_error_unknown_stream: u64,
    pub write_error_zero_rtt_rejected: u64,
}

#[allow(clippy::arithmetic_side_effects)]
pub fn record_error(err: QuicError, stats: &mut SendTransactionStats) {
    match err {
        QuicError::ConnectError(ConnectError::EndpointStopping) => {
            stats.connect_error_other += 1;
        }
        QuicError::ConnectError(ConnectError::TooManyConnections) => {
            stats.connect_error_too_many_connections += 1;
        }
        QuicError::ConnectError(ConnectError::InvalidDnsName(_)) => {
            stats.connect_error_other += 1;
        }
        QuicError::ConnectError(ConnectError::InvalidRemoteAddress(_)) => {
            stats.connect_error_invalid_remote_address += 1;
        }
        QuicError::ConnectError(ConnectError::NoDefaultClientConfig) => {
            stats.connect_error_other += 1;
        }
        QuicError::ConnectError(ConnectError::UnsupportedVersion) => {
            stats.connect_error_other += 1;
        }
        QuicError::ConnectionError(ConnectionError::VersionMismatch) => {
            stats.connection_error_version_mismatch += 1;
        }
        QuicError::ConnectionError(ConnectionError::TransportError(_)) => {
            stats.connection_error_transport_error += 1;
        }
        QuicError::ConnectionError(ConnectionError::ConnectionClosed(_)) => {
            stats.connection_error_connection_closed += 1;
        }
        QuicError::ConnectionError(ConnectionError::ApplicationClosed(_)) => {
            stats.connection_error_application_closed += 1;
        }
        QuicError::ConnectionError(ConnectionError::Reset) => {
            stats.connection_error_reset += 1;
        }
        QuicError::ConnectionError(ConnectionError::TimedOut) => {
            stats.connection_error_timed_out += 1;
        }
        QuicError::ConnectionError(ConnectionError::LocallyClosed) => {
            stats.connection_error_locally_closed += 1;
        }
        QuicError::WriteError(WriteError::Stopped(_)) => {
            stats.write_error_stopped += 1;
        }
        QuicError::WriteError(WriteError::ConnectionLost(_)) => {
            stats.write_error_connection_lost += 1;
        }
        QuicError::WriteError(WriteError::UnknownStream) => {
            stats.write_error_unknown_stream += 1;
        }
        QuicError::WriteError(WriteError::ZeroRttRejected) => {
            stats.write_error_zero_rtt_rejected += 1;
        }
        // Endpoint is created on the scheduler level and handled separately
        // No counters are used for this case.
        QuicError::EndpointError(_) => (),
    }
}

pub type SendTransactionStatsPerAddr = HashMap<IpAddr, SendTransactionStats>;

macro_rules! add_fields {
    ($self:ident, $other:ident, $( $field:ident ),* ) => {
        $(
            $self.$field = $self.$field.saturating_add($other.$field);
        )*
    };
}

impl SendTransactionStats {
    pub fn add(&mut self, other: &SendTransactionStats) {
        add_fields!(
            self,
            other,
            successfully_sent,
            connect_error_invalid_remote_address,
            connect_error_other,
            connect_error_too_many_connections,
            connection_error_application_closed,
            connection_error_connection_closed,
            connection_error_locally_closed,
            connection_error_reset,
            connection_error_timed_out,
            connection_error_transport_error,
            connection_error_version_mismatch,
            write_error_connection_lost,
            write_error_stopped,
            write_error_unknown_stream,
            write_error_zero_rtt_rejected
        );
    }
}

macro_rules! display_send_transaction_stats {
    ($($field:ident),*) => {
        impl fmt::Display for SendTransactionStats {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    f,
                    concat!(
                        "SendTransactionStats:\n",
                        $(
                            "\x20   ", stringify!($field), ": {},\n",
                        )*
                    ),
                    $(self.$field),*
                )
            }
        }
    };
}

display_send_transaction_stats!(
    successfully_sent,
    connect_error_invalid_remote_address,
    connect_error_other,
    connect_error_too_many_connections,
    connection_error_application_closed,
    connection_error_connection_closed,
    connection_error_locally_closed,
    connection_error_reset,
    connection_error_timed_out,
    connection_error_transport_error,
    connection_error_version_mismatch,
    write_error_connection_lost,
    write_error_stopped,
    write_error_unknown_stream,
    write_error_zero_rtt_rejected
);
