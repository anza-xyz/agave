#![cfg(target_os = "linux")]

use {
    solana_net_utils::bind_to_localhost,
    solana_streamer::{packet::PACKET_DATA_SIZE, recvmmsg::*, streamer::RecvBuffer},
    std::time::Instant,
};

#[test]
pub fn test_recv_mmsg_batch_size() {
    let reader = bind_to_localhost().expect("bind");
    let addr = reader.local_addr().unwrap();
    let sender = bind_to_localhost().expect("bind");

    const TEST_BATCH_SIZE: usize = 64;
    let sent = TEST_BATCH_SIZE;

    let mut elapsed_in_max_batch = 0;
    let mut num_max_batches = 0;
    (0..1000).for_each(|_| {
        for _ in 0..sent {
            let data = [0; PACKET_DATA_SIZE];
            sender.send_to(&data[..], addr).unwrap();
        }
        let mut buffer = RecvBuffer::new(TEST_BATCH_SIZE);
        let mut buffers = buffer.chunk_bufs();
        let mut metas = RecvMetas::new();
        let now = Instant::now();
        let recv = recv_mmsg(&reader, &mut buffers[..], &mut metas).unwrap();
        elapsed_in_max_batch += now.elapsed().as_nanos();
        if recv == TEST_BATCH_SIZE {
            num_max_batches += 1;
        }
    });
    assert!(num_max_batches > 990);

    let mut elapsed_in_small_batch = 0;
    (0..1000).for_each(|_| {
        for _ in 0..sent {
            let data = [0; PACKET_DATA_SIZE];
            sender.send_to(&data[..], addr).unwrap();
        }
        let mut buffer = RecvBuffer::new(4);
        let mut buffers = buffer.chunk_bufs();
        let mut metas = RecvMetas::new();
        let mut recv = 0;
        let now = Instant::now();
        while let Ok(num) = recv_mmsg(&reader, &mut buffers[..], &mut metas) {
            recv += num;
            if recv >= TEST_BATCH_SIZE {
                break;
            }
            metas.clear();
        }
        elapsed_in_small_batch += now.elapsed().as_nanos();
        assert_eq!(TEST_BATCH_SIZE, recv);
    });

    assert!(elapsed_in_max_batch <= elapsed_in_small_batch);
}
