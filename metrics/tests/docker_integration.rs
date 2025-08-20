#![cfg(feature = "docker-tests")]

use {
    serde_json::Value,
    serial_test::serial,
    solana_logger,
    solana_metrics::datapoint_info,
    std::{env, time::Duration},
    testcontainers::{core::WaitFor, runners::SyncRunner, GenericImage, ImageExt},
};

#[test]
#[serial]
fn test_influxdb_v1_with_datapoint_info() {
    solana_logger::setup_with_default("info");

    let influxdb_image = GenericImage::new("influxdb", "1.11")
        .with_wait_for(WaitFor::Duration {
            length: Duration::from_secs(5),
        })
        .with_exposed_port(8086.into())
        .with_env_var("INFLUXDB_DB", "testdb")
        .with_env_var("INFLUXDB_ADMIN_USER", "admin")
        .with_env_var("INFLUXDB_ADMIN_PASSWORD", "password");

    let container = influxdb_image.start().expect("Failed to start container");
    let host_port = container
        .get_host_port_ipv4(8086)
        .expect("Failed to get host port");
    let host_url = format!("http://localhost:{}", host_port);

    env::set_var(
        "SOLANA_METRICS_CONFIG",
        format!("host={},db=testdb,u=admin,p=password", host_url),
    );
    solana_metrics::set_host_id("test_host_id".to_string());

    datapoint_info!("test_metrics", ("A", 42, i64), ("B", 85.5, f64),);
    solana_metrics::flush();

    let query_result =
        solana_metrics::influxdb_v1::query("SELECT time, host_id, A, B FROM test_metrics")
            .expect("Failed to query databases");
    let v: Value = serde_json::from_str(&query_result).unwrap();

    let series = &v["results"][0]["series"][0];

    assert_eq!(series["name"], "test_metrics");
    assert_eq!(
        series["columns"].as_array().unwrap(),
        &vec![
            Value::from("time"),
            Value::from("host_id"),
            Value::from("A"),
            Value::from("B"),
        ]
    );
    let values = series["values"][0].as_array().unwrap();
    // values[0] is timestamp. ignore it because it's hard to get the correct value in the test.
    assert_eq!(values[1], Value::from("test_host_id"));
    assert_eq!(values[2], Value::from(42));
    assert_eq!(values[3], Value::from(85.5));

    env::remove_var("SOLANA_METRICS_CONFIG");
}

#[test]
#[serial]
fn test_influxdb_v2_with_datapoint_info() {
    solana_logger::setup_with_default("info");

    let influxdb_image = GenericImage::new("influxdb", "2.7")
        .with_wait_for(WaitFor::Duration {
            length: Duration::from_secs(5),
        })
        .with_exposed_port(8086.into())
        .with_env_var("DOCKER_INFLUXDB_INIT_MODE", "setup")
        .with_env_var("DOCKER_INFLUXDB_INIT_USERNAME", "admin")
        .with_env_var("DOCKER_INFLUXDB_INIT_PASSWORD", "password")
        .with_env_var("DOCKER_INFLUXDB_INIT_ORG", "anzaxyz")
        .with_env_var("DOCKER_INFLUXDB_INIT_BUCKET", "mydb")
        .with_env_var("DOCKER_INFLUXDB_INIT_ADMIN_TOKEN", "admin");

    let container = influxdb_image.start().expect("Failed to start container");
    let host_port = container
        .get_host_port_ipv4(8086)
        .expect("Failed to get host port");
    let host_url = format!("http://localhost:{}", host_port);

    env::set_var("SOLANA_METRICS_INFLUXDB_V1", "false");
    env::set_var("SOLANA_METRICS_INFLUXDB_V2", "true");
    env::set_var("SOLANA_METRICS_INFLUXDB_V2_HOST", host_url);
    env::set_var("SOLANA_METRICS_INFLUXDB_V2_ORG", "anzaxyz");
    env::set_var("SOLANA_METRICS_INFLUXDB_V2_BUCKET", "mydb");
    env::set_var("SOLANA_METRICS_INFLUXDB_V2_TOKEN", "admin");

    solana_metrics::set_host_id("test_host_id".to_string());
    datapoint_info!("test_metrics", ("A", 42, i64), ("B", 85.5, f64),);
    solana_metrics::flush();

    let query_result =
        solana_metrics::influxdb_v2::query("SELECT time, host_id, A, B FROM test_metrics")
            .expect("Failed to query databases");
    let v: Value = serde_json::from_str(&query_result).unwrap();

    let series = &v["results"][0]["series"][0];

    assert_eq!(series["name"], "test_metrics");
    assert_eq!(
        series["columns"].as_array().unwrap(),
        &vec![
            Value::from("time"),
            Value::from("host_id"),
            Value::from("A"),
            Value::from("B"),
        ]
    );

    let values = series["values"][0].as_array().unwrap();
    // values[0] is timestamp. ignore it because it's hard to get the correct value in the test.
    assert_eq!(values[1], Value::from("test_host_id"));
    assert_eq!(values[2], Value::from(42));
    assert_eq!(values[3], Value::from(85.5));

    env::remove_var("SOLANA_METRICS_INFLUXDB_V2");
    env::remove_var("SOLANA_METRICS_INFLUXDB_V2_HOST");
    env::remove_var("SOLANA_METRICS_INFLUXDB_V2_ORG");
    env::remove_var("SOLANA_METRICS_INFLUXDB_V2_BUCKET");
    env::remove_var("SOLANA_METRICS_INFLUXDB_V2_TOKEN");
}
