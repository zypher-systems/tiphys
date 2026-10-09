//! The daemon's hosts, one per audience.
//!
//! An audience is whoever a conversation is with: the owner at the terminal,
//! one-shot runs, one chat. Each has its own host, with its own session.
//! Hosts are started the first time something needs them and run until the
//! daemon stops.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tiphys_core::host::{ClientId, Host, Input};
use tiphys_core::llm::Connect;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

/// How long the hosts are given to stop a running turn when the daemon ends.
const STOP_WITHIN: Duration = Duration::from_secs(15);

/// A host that has been started: the way in, and the task it runs as.
type Running = (UnboundedSender<Input>, JoinHandle<()>);

pub struct Hosts {
    home: PathBuf,
    user_home: PathBuf,
    connect: Arc<dyn Connect>,
    running: Mutex<HashMap<String, Running>>,
    next_client: AtomicU64,
}

impl Hosts {
    pub fn new(home: &Path, user_home: &Path, connect: Arc<dyn Connect>) -> Self {
        Self {
            home: home.to_path_buf(),
            user_home: user_home.to_path_buf(),
            connect,
            running: Mutex::default(),
            next_client: AtomicU64::new(1),
        }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The way in to an audience's host, which is started if it is not
    /// running yet.
    pub fn inbox(&self, audience: &str) -> UnboundedSender<Input> {
        let mut running = self.running.lock().unwrap();
        if let Some((inbox, _)) = running.get(audience) {
            return inbox.clone();
        }
        let (inbox, inputs) = unbounded_channel();
        let host = Host::new(&self.home, &self.user_home, audience, self.connect.clone());
        let task = tokio::spawn(host.run(inputs));
        running.insert(audience.to_string(), (inbox.clone(), task));
        inbox
    }

    /// A number no other client of this daemon has.
    pub fn client_id(&self) -> ClientId {
        self.next_client.fetch_add(1, Ordering::Relaxed)
    }

    /// Stops every host. Whatever else holds a way in to one must have let
    /// go of it first: a host with nothing that can reach it stops its turn,
    /// so that the turn's end is on the record, and ends.
    pub async fn stop(&self) {
        let running: Vec<_> = self.running.lock().unwrap().drain().collect();
        for (_, (inbox, task)) in running {
            drop(inbox);
            let _ = tokio::time::timeout(STOP_WITHIN, task).await;
        }
    }
}
