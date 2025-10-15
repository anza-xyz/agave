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
    std::ops::{Deref, DerefMut},
};

pub struct SubscriptionWithClient<T: DeserializeOwned> {
    _client: Client,
    subscription: Subscription<T>,
}

impl<T: DeserializeOwned> Deref for SubscriptionWithClient<T> {
    type Target = Subscription<T>;

    fn deref(&self) -> &Self::Target {
        &self.subscription
    }
}

impl<T: DeserializeOwned> DerefMut for SubscriptionWithClient<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.subscription
    }
}

pub async fn subscribe<Notif, Params>(
    url: &str,
    method: &str,
    params: Params,
) -> Result<SubscriptionWithClient<Notif>, ClientError>
where
    Params: ToRpcParams + Send,
    Notif: DeserializeOwned,
{
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
    Ok(SubscriptionWithClient {
        _client: client,
        subscription,
    })
}
