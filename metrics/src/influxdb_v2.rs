use {
    crate::{
        datapoint::DataPoint,
        metrics::{MetricsWriter, HOST_ID},
    },
    log::*,
    std::{
        env,
        fmt::Write,
        time::{Duration, UNIX_EPOCH},
    },
};

#[derive(Debug)]
pub struct Config {
    pub host: String,
    pub org: String,
    pub bucket: String,
    pub token: String,
}

impl Config {
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
        let config = get_config();
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

pub fn serialize_points(points: &Vec<DataPoint>, host_id: &str) -> String {
    const TIMESTAMP_LEN: usize = 20;
    const HOST_ID_LEN: usize = 8; // "host_id=".len()
    const EXTRA_LEN: usize = 2; // "=,".len()
    let mut len = 0;
    for point in points {
        for (name, value) in &point.fields {
            len += name.len() + value.len() + EXTRA_LEN;
        }
        for (name, value) in &point.tags {
            len += name.len() + value.len() + EXTRA_LEN;
        }
        len += point.name.len();
        len += TIMESTAMP_LEN;
        len += host_id.len() + HOST_ID_LEN;
    }
    let mut line = String::with_capacity(len);
    for point in points {
        let _ = write!(line, "{},host_id={}", &point.name, host_id);
        for (name, value) in point.tags.iter() {
            let _ = write!(line, ",{name}={value}");
        }

        let mut first = true;
        for (name, value) in point.fields.iter() {
            let _ = write!(line, "{}{}={}", if first { ' ' } else { ',' }, name, value);
            first = false;
        }
        let timestamp = point.timestamp.duration_since(UNIX_EPOCH);
        let nanos = timestamp.unwrap().as_nanos();
        let _ = writeln!(line, " {nanos}");
    }
    line
}

pub fn get_config() -> Config {
    Config {
        host: env::var("SOLANA_METRICS_INFLUXDB_V2_HOST").unwrap_or_default(),
        org: env::var("SOLANA_METRICS_INFLUXDB_V2_ORG").unwrap_or_default(),
        bucket: env::var("SOLANA_METRICS_INFLUXDB_V2_BUCKET").unwrap_or_default(),
        token: env::var("SOLANA_METRICS_INFLUXDB_V2_TOKEN").unwrap_or_default(),
    }
}
