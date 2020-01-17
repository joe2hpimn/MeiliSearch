use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use log::*;
use raft::eraftpb::{ConfChange, ConfState, Entry, EntryType, Message};
use raft::storage::MemStorage;
use raft::{self, Config, RawNode};
use slog::{o, Drain};

// use crate::util;

pub enum PeerMessage {
    Propose(Vec<u8>),
    Message(Message),
    ConfChange(ConfChange),
}

pub struct Peer {
    pub node: RawNode<MemStorage>,
    apply_ch: SyncSender<Entry>,
}

impl Peer {
    pub fn new(id: u64, apply_ch: SyncSender<Entry>) -> Peer {
        let cfg = default_raft_config(id);
        let conf_state = ConfState::from((vec![id], vec![]));
        let storge = MemStorage::new_with_conf_state(conf_state);
        let logger = slog::Logger::root(slog_stdlog::StdLog.fuse(), o!());
        let node = RawNode::new(&cfg, storge, &logger).unwrap();
        Peer {
            node,
            apply_ch,
        }
    }

    pub fn activate(mut peer: Peer, sender: SyncSender<Message>, receiver: Receiver<PeerMessage>) {
        thread::spawn(move || {
            peer.listen_message(sender, receiver);
        });
    }

    // Ticking function
    fn listen_message(&mut self, sender: SyncSender<Message>, receiver: Receiver<PeerMessage>) {
        let mut t = Instant::now();
        let mut timeout = Duration::from_millis(100);
        loop {
            match receiver.recv_timeout(timeout) {
                Ok(PeerMessage::Propose(p)) => match self.node.propose(vec![], p) {
                    Ok(_) => (),
                    Err(_err) => self.apply_message(Entry::default()),
                },
                Ok(PeerMessage::ConfChange(cc)) => {
                    match self.node.propose_conf_change(vec![], cc.clone()) {
                        Ok(_) => (),
                        Err(_err) => error!("conf change failed: {:?}", cc),
                    }
                }
                Ok(PeerMessage::Message(m)) => self.node.step(m).unwrap(),
                Err(RecvTimeoutError::Timeout) => (),
                Err(RecvTimeoutError::Disconnected) => return,
            }

            let d = t.elapsed();
            if d >= timeout {
                t = Instant::now();
                timeout = Duration::from_millis(200);
                self.node.tick();
            } else {
                timeout -= d;
            }

            self.on_ready(sender.clone());
        }
    }

    pub fn is_leader(&self) -> bool {
        self.node.raft.leader_id == self.node.raft.id
    }

    fn on_ready(&mut self, sender: SyncSender<Message>) {
        if !self.node.has_ready() {
            return;
        }

        let mut ready = self.node.ready();
        let is_leader = self.node.raft.leader_id == self.node.raft.id;
        if is_leader {
            let msgs = ready.messages.drain(..);
            for _msg in msgs {
                Self::send_message(sender.clone(), _msg.clone());
            }
        }

        if !raft::is_empty_snap(&ready.snapshot()) {
            self.node
                .mut_store()
                .wl()
                .apply_snapshot(ready.snapshot().clone())
                .unwrap()
        }

        if !ready.entries().is_empty() {
            self.node.mut_store().wl().append(&ready.entries()).unwrap();
        }

        if let Some(hs) = ready.hs() {
            self.node.mut_store().wl().set_hardstate(hs.clone());
        }

        if !is_leader {
            let msgs = ready.messages.drain(..);
            for mut _msg in msgs {
                for _entry in _msg.mut_entries().iter() {
                    if _entry.get_entry_type() == EntryType::EntryConfChange {}
                }
                Self::send_message(sender.clone(), _msg.clone());
            }
        }

        if let Some(committed_entries) = ready.committed_entries.take() {
            let mut _last_apply_index = 0;
            for entry in committed_entries {
                // Mostly, you need to save the last apply index to resume applying
                // after restart. Here we just ignore this because we use a Memory storage.
                _last_apply_index = entry.get_index();

                if entry.get_data().is_empty() {
                    // Emtpy entry, when the peer becomes Leader it will send an empty entry.
                    continue;
                }

                match entry.get_entry_type() {
                    EntryType::EntryNormal => self.apply_message(entry.clone()),
                    EntryType::EntryConfChange => {
                        let cc = util::parse_data(&entry.data);
                        debug!("config: {:?}", cc);
                        if let Err(err) = self.node.apply_conf_change(&cc) {
                            warn!("conf change cannot be applied; {}", err);
                        } else {
                            debug!("apply conf change");
                        };
                        self.apply_message(entry.clone());
                    }
                    EntryType::EntryConfChangeV2 => {
                        unimplemented!();
                    }
                }
            }
        }

        // Advance the Raft
        self.node.advance(ready);
    }

    fn send_message(sender: SyncSender<Message>, msg: Message) {
        thread::spawn(move || {
            sender.send(msg).unwrap_or_else(|e| {
                panic!("raft send message error: {}", e);
            });
        });
    }

    fn apply_message(&self, entry: Entry) {
        let sender = self.apply_ch.clone();
        thread::spawn(move || {
            sender.send(entry).unwrap_or_else(|e| {
                panic!("raft send apply entry error: {}", e);
            });
        });
    }
}

pub fn default_raft_config(id: u64) -> Config {
    debug!("default_raft_config id:{}", id);
    Config {
        id,
        election_tick: 10,
        heartbeat_tick: 1,
        max_size_per_msg: 1024 * 1024 * 1024,
        max_inflight_msgs: 256,
        applied: 0,
        ..Default::default()
    }
}