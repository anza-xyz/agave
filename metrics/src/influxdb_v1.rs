use {
    crate::{
        datapoint::DataPoint,
        influxdb_common::serialize_points,
        metrics::{MetricsError, MetricsWriter, HOST_ID},
    },
    log::*,
    std::{env, time::Duration},
};

#[derive(Debug, Default)]
pub struct MetricsConfig {
    pub host: String,
    pub db: String,
    pub username: String,
    pub password: String,
}

impl MetricsConfig {
    fn complete(&self) -> bool {
        !(self.host.is_empty()
            || self.db.is_empty()
            || self.username.is_empty()
            || self.password.is_empty())
    }
}

#[deprecated(
    since = "3.0.0",
    note = "DEPRECATED: Use `Writer` instead. This alias will be removed in the future."
)]
pub type InfluxDbMetricsWriter = Writer;

pub struct Writer {
    write_url: Option<String>,
}

impl Writer {
    pub fn new() -> Self {
        Self {
            write_url: Self::build_write_url().ok(),
        }
    }

    fn build_write_url() -> Result<String, MetricsError> {
        let config = get_metrics_config().map_err(|err| {
            info!("metrics disabled: {}", err);
            err
        })?;

        info!(
            "metrics configuration: host={} db={} username={}",
            config.host, config.db, config.username
        );

        let write_url = format!(
            "{}/write?db={}&u={}&p={}&precision=n",
            &config.host, &config.db, &config.username, &config.password
        );

        Ok(write_url)
    }
}

impl MetricsWriter for Writer {
    fn write(&self, points: Vec<DataPoint>) {
        if let Some(ref write_url) = self.write_url {
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

            let response = client.post(write_url.as_str()).body(line).send();
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
        }
    }
}

pub fn get_metrics_config() -> Result<MetricsConfig, MetricsError> {
    let mut config = MetricsConfig::default();
    let config_var = env::var("SOLANA_METRICS_CONFIG")?;
    if config_var.is_empty() {
        Err(env::VarError::NotPresent)?;
    }

    for pair in config_var.split(',') {
        let nv: Vec<_> = pair.split('=').collect();
        if nv.len() != 2 {
            return Err(MetricsError::ConfigInvalid(pair.to_string()));
        }
        let v = nv[1].to_string();
        match nv[0] {
            "host" => config.host = v,
            "db" => config.db = v,
            "u" => config.username = v,
            "p" => config.password = v,
            _ => return Err(MetricsError::ConfigInvalid(pair.to_string())),
        }
    }

    if !config.complete() {
        return Err(MetricsError::ConfigIncomplete);
    }

    Ok(config)
}

pub fn query(q: &str) -> Result<String, MetricsError> {
    let config = get_metrics_config()?;
    let query_url = format!(
        "{}/query?u={}&p={}&q={}",
        &config.host, &config.username, &config.password, &q
    );

    let response = reqwest::blocking::get(query_url.as_str())?.text()?;

    Ok(response)
}
