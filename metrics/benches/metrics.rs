use {
    bencher::{benchmark_group, benchmark_main, Bencher},
    log::*,
    rand::distr::{Distribution, Uniform},
    solana_metrics::{
        counter::CounterPoint,
        datapoint::DataPoint,
        metrics::{serialize_points, test_mocks::MockMetricsWriter, MetricsAgent},
    },
    std::{hint::black_box, sync::Arc, time::Duration},
};

fn bench_write_points(b: &mut Bencher) {
    let points = (0..10)
        .map(|_| {
            DataPoint::new("measurement")
                .add_field_i64("i", 0)
                .add_field_i64("abc123", 2)
                .add_field_i64("this-is-my-very-long-field-name", 3)
                .clone()
        })
        .collect();
    let host_id = "benchmark-host-id";
    b.iter(|| {
        for _ in 0..10 {
            black_box(serialize_points(&points, host_id));
        }
    })
}

fn bench_datapoint_submission(b: &mut Bencher) {
    let writer = Arc::new(MockMetricsWriter::new());
    let agent = MetricsAgent::new(writer, Duration::from_secs(10), 1000);

    b.iter(|| {
        for i in 0..1000 {
            agent.submit(
                DataPoint::new("measurement")
                    .add_field_i64("i", i)
                    .to_owned(),
                Level::Info,
            );
        }
        agent.flush();
    })
}

fn bench_counter_submission(b: &mut Bencher) {
    let writer = Arc::new(MockMetricsWriter::new());
    let agent = MetricsAgent::new(writer, Duration::from_secs(10), 1000);

    b.iter(|| {
        for i in 0..1000 {
            agent.submit_counter(CounterPoint::new("counter 1"), Level::Info, i);
        }
        agent.flush();
    })
}

fn bench_random_submission(b: &mut Bencher) {
    let writer = Arc::new(MockMetricsWriter::new());
    let agent = MetricsAgent::new(writer, Duration::from_secs(10), 1000);
    let mut rng = rand::rng();
    let die = Uniform::<i32>::try_from(1..7).expect("ok for non-empty range");

    b.iter(|| {
        for i in 0..1000 {
            let dice = die.sample(&mut rng);

            if dice == 6 {
                agent.submit_counter(CounterPoint::new("counter 1"), Level::Info, i);
            } else {
                agent.submit(
                    DataPoint::new("measurement")
                        .add_field_i64("i", i as i64)
                        .to_owned(),
                    Level::Info,
                );
            }
        }
        agent.flush();
    })
}

benchmark_group!(
    benches,
    bench_write_points,
    bench_datapoint_submission,
    bench_counter_submission,
    bench_random_submission
);
benchmark_main!(benches);
