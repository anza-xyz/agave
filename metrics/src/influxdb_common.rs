use {
    crate::datapoint::DataPoint,
    std::{fmt::Write, time::UNIX_EPOCH},
};

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
