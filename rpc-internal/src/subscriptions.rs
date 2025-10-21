#![cfg(feature = "subscriptions")]

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

impl<T: DeserializeOwned> SubscriptionWithClient<T> {
    pub async fn unsubscribe(self) -> Result<(), ClientError> {
        self.subscription.unsubscribe().await
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
        .max_buffer_capacity_per_subscription(
            // Gets us as close to 'unbounded' as is possible with `tokio::sync::mpsc::channel`.
            2_305_843_009_213_693_951usize, /* Semaphore::MAX_PERMITS */
        )
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

#[cfg(test)]
mod tests {
    use {
        crate::subscriptions::{subscribe, SubscriptionWithClient},
        serde_json::json,
        ws_mock::{
            matchers::JsonPartial,
            ws_mock_server::{WsMock, WsMockServer},
        },
    };

    #[tokio::test]
    async fn test_sends_subscribe_message() {
        let server = WsMockServer::start().await;

        WsMock::new()
            .matcher(JsonPartial::new(json!({ "method": "slotSubscribe" })))
            .respond_with(
                json!({ "jsonrpc": "2.0", "id": 0, "result": 123 })
                    .to_string()
                    .into(),
            )
            .expect(1)
            .mount(&server)
            .await;

        let _subscription: SubscriptionWithClient<()> =
            subscribe(&server.uri().await, "slot", vec![] as Vec<()>)
                .await
                .expect("Failed to establish subscription");

        server.verify().await;
    }

    #[tokio::test]
    async fn test_unsubscribes_from_received_subscription_id() {
        let server = WsMockServer::start().await;

        WsMock::new()
            .matcher(JsonPartial::new(json!({ "method": "slotSubscribe" })))
            .respond_with(
                json!({ "jsonrpc": "2.0", "id": 0, "result": 123 })
                    .to_string()
                    .into(),
            )
            .mount(&server)
            .await;

        let subscription: SubscriptionWithClient<()> =
            subscribe(&server.uri().await, "slot", vec![] as Vec<()>)
                .await
                .expect("Failed to establish subscription");

        WsMock::new()
            .matcher(JsonPartial::new(json!({ "method": "slotUnsubscribe" })))
            .respond_with(
                json!({ "jsonrpc": "2.0", "id": 1, "result": true })
                    .to_string()
                    .into(),
            )
            .expect(1)
            .mount(&server)
            .await;

        subscription
            .unsubscribe()
            .await
            .expect("Failed to unsubscribe");

        server.verify().await;
    }

    #[tokio::test]
    async fn test_receives_notifications() {
        let server = WsMockServer::start().await;

        WsMock::new()
            .matcher(JsonPartial::new(json!({ "method": "slotSubscribe" })))
            .respond_with(
                json!({ "jsonrpc": "2.0", "id": 0, "result": 123 })
                    .to_string()
                    .into(),
            )
            .respond_with(
                json!({
                    "jsonrpc": "2.0",
                    "method": "slotNotification",
                    "params": { "result": "foo", "subscription": 123 }
                })
                .to_string()
                .into(),
            )
            .respond_with(
                json!({
                    "jsonrpc": "2.0",
                    "method": "slotNotification",
                    "params": { "result": "bar", "subscription": 123 }
                })
                .to_string()
                .into(),
            )
            .respond_with(
                json!({
                    "jsonrpc": "2.0",
                    "method": "slotNotification",
                    "params": { "result": "baz", "subscription": 123 }
                })
                .to_string()
                .into(),
            )
            .mount(&server)
            .await;

        let mut subscription: SubscriptionWithClient<String> =
            subscribe(&server.uri().await, "slot", vec![] as Vec<()>)
                .await
                .expect("Failed to establish subscription");

        let mut notifications = vec![];
        for _ in 0..3 {
            let notification = subscription
                .next()
                .await
                .expect("Received no notification")
                .unwrap();
            notifications.push(notification);
        }

        assert_eq!(notifications, vec!["foo", "bar", "baz"]);
    }
}
