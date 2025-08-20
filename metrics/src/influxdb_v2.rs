use {
    crate::{
        datapoint::DataPoint,
        influxdb_common::serialize_points,
        metrics::{MetricsError, MetricsWriter, HOST_ID},
    },
    log::*,
    std::{env, time::Duration},
};

#[derive(Debug)]
pub struct Config {
    pub host: String,
    pub org: String,
    pub bucket: String,
    pub token: String,
}

impl Config {
    fn from_env() -> Self {
        Self {
            host: env::var("SOLANA_METRICS_INFLUXDB_V2_HOST").unwrap_or_default(),
            org: env::var("SOLANA_METRICS_INFLUXDB_V2_ORG").unwrap_or_default(),
            bucket: env::var("SOLANA_METRICS_INFLUXDB_V2_BUCKET").unwrap_or_default(),
            token: env::var("SOLANA_METRICS_INFLUXDB_V2_TOKEN").unwrap_or_default(),
        }
    }

    fn complete(&self) -> bool {
        !(self.host.is_empty()
            || self.org.is_empty()
            || self.bucket.is_empty()
            || self.token.is_empty())
    }
}

pub struct Writer {
    url: Option<String>,
    token: Option<String>,
}

impl Writer {
    pub fn new() -> Self {
        let config = Config::from_env();
        if !config.complete() {
            return Self {
                url: None,
                token: None,
            };
        }

        Self {
            url: Some(format!(
                "{}/api/v2/write?org={}&bucket={}&precision=ns",
                config.host, config.org, config.bucket
            )),
            token: Some(config.token),
        }
    }
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsWriter for Writer {
    fn write(&self, points: Vec<DataPoint>) {
        if let Some(ref url) = self.url {
            debug!("submitting {} points", points.len());

            let host_id = HOST_ID.read().unwrap();

            let line = serialize_points(&points, &host_id);

            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(5))
                .build();
            let client = match client {
                Ok(client) => client,
                Err(err) => {
                    warn!("client instantiation failed: {}", err);
                    return;
                }
            };

            let mut request = client.post(url.as_str()).body(line);

            if let Some(ref token) = self.token {
                request = request.header("Authorization", format!("Token {}", token));
            }

            let response = request.send();
            if let Ok(resp) = response {
                let status = resp.status();
                if !status.is_success() {
                    let text = resp
                        .text()
                        .unwrap_or_else(|_| "[text body empty]".to_string());
                    warn!("submit response unsuccessful: {} {}", status, text,);
                }
            } else {
                warn!("submit error: {}", response.unwrap_err());
            }
        } else {
            warn!("tried to submit points to influxdb v2 but no url was set");
        }
    }
}

// https://docs.influxdata.com/influxdb/v2/query-data/execute-queries/influx-api/#influxql---example-query-request
pub fn query(q: &str) -> Result<String, MetricsError> {
    let config = Config::from_env();
    let query_url = format!(
        "{}/query?db={}&p={}&u=ignored&q={}",
        &config.host, &config.bucket, &config.token, &q
    );

    let response = reqwest::blocking::get(query_url.as_str())?.text()?;

    Ok(response)
}
