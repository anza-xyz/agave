use {
    jsonrpsee::{
        core::{
            client::{async_client::Client, Subscription, SubscriptionClientT},
            traits::ToRpcParams,
            ClientError,
        },
        ws_client::{PingConfig, WsClientBuilder},
    },
    serde::de::DeserializeOwned,
    tokio::runtime::{Builder, Runtime},
};

pub struct SubscriptionWithClient<T: DeserializeOwned> {
    _client: Client,
    runtime: Runtime,
    subscription: Subscription<T>,
}

impl<T: DeserializeOwned> SubscriptionWithClient<T> {
    pub fn recv(&mut self) -> Result<T, serde_json::Error> {
        self.runtime.block_on(async {
            loop {
                match self.subscription.next().await {
                    None => continue,
                    Some(notif) => return notif,
                }
            }
        })
    }
}

pub fn subscribe<Notif, Params>(
    url: &str,
    method: &str,
    params: Params,
) -> Result<SubscriptionWithClient<Notif>, ClientError>
where
    Params: ToRpcParams + Send,
    Notif: DeserializeOwned,
{
    let runtime = Builder::new_current_thread()
        .enable_time()
        .enable_io()
        .build()
        .unwrap();
    let (client, subscription) = runtime.block_on(async {
        let client = WsClientBuilder::default()
            .enable_ws_ping(PingConfig::default())
            .build(url)
            .await?;
        let subscription = client
            .subscribe(
                format!("{method}Subscribe").as_str(),
                params,
                format!("{method}Unsubscribe").as_str(),
            )
            .await?;
        Ok::<_, jsonrpsee::core::ClientError>((client, subscription))
    })?;
    Ok(SubscriptionWithClient {
        _client: client,
        runtime,
        subscription,
    })
}
