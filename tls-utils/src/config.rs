use {
    rustls::{
        ClientConfig, ConfigBuilder, ServerConfig, client::WantsClientCert, server::WantsServerCert,
    },
    std::sync::Arc,
};

pub fn tls_client_config_builder() -> ConfigBuilder<ClientConfig, WantsClientCert> {
    ClientConfig::builder_with_provider(Arc::new(crate::crypto_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(crate::SkipServerVerification::new())
}

pub fn tls_server_config_builder() -> ConfigBuilder<ServerConfig, WantsServerCert> {
    ServerConfig::builder_with_provider(Arc::new(crate::crypto_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_client_cert_verifier(crate::SkipClientVerification::new())
}
