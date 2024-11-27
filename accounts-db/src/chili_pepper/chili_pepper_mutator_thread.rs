use {
    super::chili_pepper_store::{ChiliPepperStoreWrapper, PubkeySlot},
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender},
    log::{debug, info},
    solana_measure::measure_us,
    solana_sdk::{clock::Slot, pubkey::Pubkey, timing::AtomicInterval},
    std::{
        path::PathBuf,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread,
        time::Duration,
    },
};

pub enum ChiliPepperMutatorThreadCommand {
    Insert((Vec<Pubkey>, Slot, u64), Sender<()>),
    Delete(Vec<Pubkey>, Slot),
    Clean(u64),
    CreateSavePoint(Sender<u64>),
    DeleteSavePoint(u64),
    Snapshot(u64, PathBuf),
}

#[derive(Debug, Default)]
pub struct ChiliPepperStat {
    last_report: AtomicInterval,
    insert_time: u64,
    clean_time: u64,
    snapshot_time: u64,
    len: u64,
    bytes: u64,
}

impl ChiliPepperStat {
    fn report(&mut self, store: &ChiliPepperStoreWrapper) {
        if self.last_report.should_update(1000) {
            self.len = store.len().unwrap_or(0);
            self.bytes = match store.stat() {
                Ok(stat) => stat.stored_bytes(),
                Err(_) => 0,
            };

            datapoint_info!(
                "chili_pepper_stat",
                ("len", self.len, i64),
                ("bytes", self.bytes, i64),
                ("insert_time", self.insert_time, i64),
                ("clean_time", self.clean_time, i64),
                ("snapshot_time", self.snapshot_time, i64),
            );

            self.reset();
        }
    }

    fn reset(&mut self) {
        self.insert_time = 0;
        self.clean_time = 0;
        self.snapshot_time = 0;
    }
}

#[derive(Debug)]
pub(super) struct ChiliPepperMutatorThread {
    pub _thread: thread::JoinHandle<()>,
}

impl ChiliPepperMutatorThread {
    pub fn new(
        receiver: Receiver<ChiliPepperMutatorThreadCommand>,
        store: Arc<ChiliPepperStoreWrapper>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        info!("ChiliPepperMutatorThread: started");
        let thread = thread::Builder::new()
            .name("solChiliPepperMutatorThread".to_string())
            .spawn(move || {
                let mut stat = ChiliPepperStat::default();

                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }
                    stat.report(&store);
                    // sleep wait on the channel for 200ms half of the block time
                    match receiver.recv_timeout(Duration::from_millis(200)) {
                        Ok(command) => match command {
                            ChiliPepperMutatorThreadCommand::Insert(
                                (ref pubkeys, slot, chili_pepper_clock),
                                sender,
                            ) => {
                                let (_, insert_time) = measure_us!({
                                    let to_insert = pubkeys.iter().map(|pubkey| {
                                        (PubkeySlot::new(pubkey, slot), chili_pepper_clock)
                                    });

                                    // TODO handle insert error
                                    store
                                        .bulk_insert(to_insert)
                                        .expect("chili pepper store insert failed");

                                    sender.send(()).unwrap();
                                });
                                stat.insert_time += insert_time;
                            }
                            ChiliPepperMutatorThreadCommand::Delete(ref keys, slot) => {
                                let iter = keys.iter().map(|pubkey| PubkeySlot::new(pubkey, slot));
                                store
                                    .bulk_remove(iter)
                                    .expect("chili pepper store delete failed");
                            }
                            ChiliPepperMutatorThreadCommand::Clean(threshold) => {
                                let (_, clean_time) = measure_us!({
                                    // TODO handle clean error
                                    store
                                        .clean(threshold)
                                        .expect("chili pepper store clean failed");
                                });
                                stat.clean_time += clean_time;
                            }
                            ChiliPepperMutatorThreadCommand::CreateSavePoint(sender) => {
                                // TOD handle savepoint error
                                let savepoint_id = store
                                    .create_savepoint()
                                    .expect("chiili pepper store create savepoint failed");
                                sender.send(savepoint_id).unwrap();
                            }
                            ChiliPepperMutatorThreadCommand::DeleteSavePoint(savepoint_id) => {
                                // TODO handle delete savepoint error
                                store
                                    .remove_savepoint(savepoint_id)
                                    .expect("chili pepper store delete savepoint failed");
                            }
                            ChiliPepperMutatorThreadCommand::Snapshot(savepoint_id, ref path) => {
                                // TODO handle snapshot error
                                store
                                    .snapshot(savepoint_id, path)
                                    .expect("chili pepper store snapshot failed");
                            }
                        },

                        Err(RecvTimeoutError::Timeout) => (),
                        Err(RecvTimeoutError::Disconnected) => {
                            debug!("ChiliPepperMutatorThread: sender disconnected");
                            break;
                        }
                    }
                }
                info!("ChiliPepperMutatorThread: exited");
            })
            .unwrap();

        Self { _thread: thread }
    }

    #[cfg(test)]
    pub fn join(self) -> thread::Result<()> {
        self._thread.join()
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crossbeam_channel::{bounded, unbounded},
        std::sync::atomic::AtomicBool,
    };

    #[test]
    fn test_chili_pepper_mutator_thread() {
        let (sender, receiver) = unbounded();
        let tmpfile = tempfile::NamedTempFile::new_in("/tmp").unwrap();
        let store = Arc::new(
            ChiliPepperStoreWrapper::new_with_path(tmpfile.path()).expect("create db success"),
        );
        let exit = Arc::new(AtomicBool::new(false));
        let mutator_thread = ChiliPepperMutatorThread::new(receiver, store.clone(), exit.clone());

        let mut pks = vec![];
        for i in 0..10 {
            pks.push(Pubkey::from([i; 32]));
        }

        let (reply_sender, reply_receiver) = bounded(1);
        let insert_cmd =
            ChiliPepperMutatorThreadCommand::Insert((pks.clone(), 10, 100), reply_sender);
        sender.send(insert_cmd).unwrap();
        reply_receiver.recv().unwrap();
        assert_eq!(store.len().unwrap(), 10);

        let (reply_sender, reply_receiver) = bounded(1);
        let insert_cmd =
            ChiliPepperMutatorThreadCommand::Insert((pks.clone(), 20, 200), reply_sender);
        sender.send(insert_cmd).unwrap();
        reply_receiver.recv().unwrap();
        assert_eq!(store.len().unwrap(), 20);

        let delete_cmd = ChiliPepperMutatorThreadCommand::Delete(pks.clone(), 10);
        sender.send(delete_cmd).unwrap();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(store.len().unwrap(), 10);

        let clean_cmd = ChiliPepperMutatorThreadCommand::Clean(300);
        sender.send(clean_cmd).unwrap();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(store.len().unwrap(), 0);

        let (reply_sender, reply_receiver) = bounded(1);
        let insert_cmd =
            ChiliPepperMutatorThreadCommand::Insert((pks.clone(), 20, 200), reply_sender);
        sender.send(insert_cmd).unwrap();
        reply_receiver.recv().unwrap();
        assert_eq!(store.len().unwrap(), 10);

        let (reply_sender, reply_receiver) = bounded(1);
        let savepoint_cmd = ChiliPepperMutatorThreadCommand::CreateSavePoint(reply_sender);
        sender.send(savepoint_cmd).unwrap();
        let savepoint = reply_receiver.recv().unwrap();

        let snapshot_path = tmpfile.path().with_extension("snapshot");
        let snapshot_cmd =
            ChiliPepperMutatorThreadCommand::Snapshot(savepoint, snapshot_path.clone());
        sender.send(snapshot_cmd).unwrap();
        thread::sleep(Duration::from_millis(1000));

        let store2 =
            Arc::new(ChiliPepperStoreWrapper::new_with_path(snapshot_path.clone()).unwrap());
        assert_eq!(store2.len().unwrap(), 10);
        std::fs::remove_file(&snapshot_path).unwrap();

        let (reply_sender, reply_receiver) = bounded(1);
        let savepoint_cmd = ChiliPepperMutatorThreadCommand::CreateSavePoint(reply_sender);
        sender.send(savepoint_cmd).unwrap();
        let savepoint = reply_receiver.recv().unwrap();

        let delete_savepoint_cmd = ChiliPepperMutatorThreadCommand::DeleteSavePoint(savepoint);
        sender.send(delete_savepoint_cmd).unwrap();
        thread::sleep(Duration::from_millis(100));

        exit.store(true, Ordering::Relaxed);
        mutator_thread.join().unwrap();
    }
}
