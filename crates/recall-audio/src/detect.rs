//! Watches the PipeWire registry for applications opening the microphone.
//!
//! A capture stream shows up as a Node global with `media.class` of
//! `Stream/Input/Audio`. Its global props carry `application.name` and
//! `client.id` but not the process binary, which only the owning client's
//! info has. So the detector binds every Client global and reports a stream
//! once its client's info has arrived.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::mpsc;
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow};
use pipewire as pw;
use pw::client::{Client, ClientListener};
use pw::types::ObjectType;

use crate::state::{DetectEvent, MicUser};

const BINARY_KEY: &str = "application.process.binary";
const APP_NAME_KEY: &str = "application.name";

/// Reports microphone streams as [`DetectEvent::MicOpened`] and
/// [`DetectEvent::MicClosed`] from a thread running its own PipeWire main
/// loop. Streams that are already open when it starts are reported too.
///
/// If the PipeWire connection fails, the detector sends
/// [`DetectEvent::DetectorDied`] and drops its sender, so the receiver also
/// sees the channel disconnect. The owner restarts it.
pub struct Detector {
    quit: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Detector {
    /// Starts watching. Streams whose `application.name` equals
    /// `ignore_app_name` (recall's own capture streams) are not reported.
    pub fn spawn(tx: mpsc::Sender<DetectEvent>, ignore_app_name: &str) -> Result<Detector> {
        let ignore = ignore_app_name.to_owned();
        let (quit, quit_rx) = pw::channel::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("recall-detect".into())
            .spawn(move || run(tx, ignore, quit_rx, ready_tx))
            .context("spawning the detector thread")?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Detector {
                quit,
                thread: Some(thread),
            }),
            Ok(Err(err)) => {
                let _ = thread.join();
                Err(err)
            }
            Err(_) => {
                let _ = thread.join();
                Err(anyhow!("the detector thread exited during startup"))
            }
        }
    }

    pub fn stop(self) {
        // Drop quits the loop and joins.
    }
}

impl Drop for Detector {
    fn drop(&mut self) {
        let _ = self.quit.send(());
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::error!("the detector thread panicked");
        }
    }
}

struct ClientEntry {
    _proxy: Client,
    _listener: ClientListener,
    /// `None` until the client's info arrives; empty if it has no binary.
    binary: Option<String>,
}

/// A stream waiting for its client's info.
struct PendingStream {
    client_id: u32,
    app_name: String,
}

struct Tracker {
    tx: mpsc::Sender<DetectEvent>,
    clients: HashMap<u32, ClientEntry>,
    pending: HashMap<u32, PendingStream>,
    /// Streams reported as opened and not yet as closed.
    reported: HashSet<u32>,
}

impl Tracker {
    fn open(&mut self, node_id: u32, binary: String, app_name: String) {
        self.reported.insert(node_id);
        tracing::debug!(node_id, %binary, %app_name, "mic stream opened");
        let _ = self.tx.send(DetectEvent::MicOpened(MicUser {
            node_id,
            binary,
            app_name,
        }));
    }

    fn client_info(&mut self, client_id: u32, binary: String) {
        let Some(entry) = self.clients.get_mut(&client_id) else {
            return;
        };
        entry.binary = Some(binary.clone());
        let ready: Vec<u32> = self
            .pending
            .iter()
            .filter(|(_, stream)| stream.client_id == client_id)
            .map(|(node_id, _)| *node_id)
            .collect();
        for node_id in ready {
            let stream = self.pending.remove(&node_id).expect("listed above");
            self.open(node_id, binary.clone(), stream.app_name);
        }
    }

    fn removed(&mut self, id: u32) {
        if self.reported.remove(&id) {
            tracing::debug!(node_id = id, "mic stream closed");
            let _ = self.tx.send(DetectEvent::MicClosed(id));
        }
        self.pending.remove(&id);
        self.clients.remove(&id);
    }
}

fn run(
    tx: mpsc::Sender<DetectEvent>,
    ignore: String,
    quit_rx: pw::channel::Receiver<()>,
    ready: mpsc::Sender<Result<()>>,
) {
    pw::init();
    let setup = || -> Result<_> {
        let main_loop = pw::main_loop::MainLoopRc::new(None).context("creating a main loop")?;
        let context =
            pw::context::ContextRc::new(&main_loop, None).context("creating a context")?;
        let core = context.connect_rc(None).context("connecting to PipeWire")?;
        let registry = core.get_registry_rc().context("getting the registry")?;
        Ok((main_loop, context, core, registry))
    };
    let (main_loop, _context, core, registry) = match setup() {
        Ok(objects) => objects,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };

    let quit_requested = Rc::new(Cell::new(false));
    let main_loop_weak = main_loop.downgrade();
    let requested = quit_requested.clone();
    let _quit = quit_rx.attach(main_loop.loop_(), move |()| {
        requested.set(true);
        if let Some(main_loop) = main_loop_weak.upgrade() {
            main_loop.quit();
        }
    });
    let core_error: Rc<RefCell<Option<String>>> = Rc::default();
    let main_loop_weak = main_loop.downgrade();
    let error_slot = core_error.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _seq, res, message| {
            tracing::error!(id, res, message, "PipeWire error in the detector");
            if id == pw::core::PW_ID_CORE {
                error_slot.borrow_mut().get_or_insert_with(|| {
                    format!("PipeWire connection failed ({res}): {message}")
                });
                if let Some(main_loop) = main_loop_weak.upgrade() {
                    main_loop.quit();
                }
            }
        })
        .register();

    let tracker = Rc::new(RefCell::new(Tracker {
        tx,
        clients: HashMap::new(),
        pending: HashMap::new(),
        reported: HashSet::new(),
    }));

    let registry_weak = registry.downgrade();
    let on_global = Rc::downgrade(&tracker);
    let on_remove = Rc::downgrade(&tracker);
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            let (Some(tracker), Some(registry)) = (on_global.upgrade(), registry_weak.upgrade())
            else {
                return;
            };
            let props = global.props;
            let prop = |key| props.and_then(|p| p.get(key));
            match global.type_ {
                ObjectType::Client => {
                    let client: Client = match registry.bind(global) {
                        Ok(client) => client,
                        Err(err) => {
                            tracing::warn!(id = global.id, %err, "binding a client");
                            return;
                        }
                    };
                    let client_id = global.id;
                    let weak = Rc::downgrade(&tracker);
                    let listener = client
                        .add_listener_local()
                        .info(move |info| {
                            let Some(props) = info.props() else { return };
                            let binary = props.get(BINARY_KEY).unwrap_or("").to_owned();
                            if let Some(tracker) = weak.upgrade() {
                                tracker.borrow_mut().client_info(client_id, binary);
                            }
                        })
                        .register();
                    tracker.borrow_mut().clients.insert(
                        client_id,
                        ClientEntry {
                            _proxy: client,
                            _listener: listener,
                            binary: None,
                        },
                    );
                }
                ObjectType::Node if prop("media.class") == Some("Stream/Input/Audio") => {
                    let app_name = prop(APP_NAME_KEY).unwrap_or("").to_owned();
                    if app_name == ignore {
                        return;
                    }
                    let mut tracker = tracker.borrow_mut();
                    if let Some(binary) = prop(BINARY_KEY) {
                        tracker.open(global.id, binary.to_owned(), app_name);
                        return;
                    }
                    let client_id = prop("client.id").and_then(|id| id.parse::<u32>().ok());
                    let known = client_id.and_then(|id| tracker.clients.get(&id));
                    match (client_id, known.map(|c| c.binary.clone())) {
                        (_, Some(Some(binary))) => tracker.open(global.id, binary, app_name),
                        (Some(client_id), Some(None)) => {
                            tracker.pending.insert(
                                global.id,
                                PendingStream {
                                    client_id,
                                    app_name,
                                },
                            );
                        }
                        // The client is not visible to us; report without
                        // a binary rather than never.
                        _ => tracker.open(global.id, String::new(), app_name),
                    }
                }
                _ => {}
            }
        })
        .global_remove(move |id| {
            if let Some(tracker) = on_remove.upgrade() {
                tracker.borrow_mut().removed(id);
            }
        })
        .register();

    let _ = ready.send(Ok(()));
    main_loop.run();
    if !quit_requested.get() {
        let reason = core_error
            .borrow_mut()
            .take()
            .unwrap_or_else(|| "the PipeWire main loop ended unexpectedly".into());
        tracing::error!(%reason, "detector died");
        let _ = tracker.borrow().tx.send(DetectEvent::DetectorDied(reason));
    }
    // Returning drops the tracker and with it the event sender, so the
    // receiver sees the channel disconnect.
}
