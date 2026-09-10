//! The controller over BlueZ's D-Bus GATT API, read on its own thread.
//!
//! BlueZ exposes every GATT characteristic of a connected device as an object under
//! `/org/bluez/hciN/dev_XX_..`, and a `StartNotify` on the report characteristic makes each
//! notification arrive as a `PropertiesChanged` on its `Value`. That is the whole transport:
//! no HCI socket, no async runtime, one system-bus connection and one match rule covering
//! the device's subtree, which also carries the device's own `Connected` property, so a
//! controller going to sleep and coming back is seen on the same iterator.
//!
//! The thread blocks in the signal iterator. `StopNotify` flips the characteristic's
//! `Notifying` property, which is itself a `PropertiesChanged` on the same path, so
//! stopping is: set the flag, call `StopNotify`, and the wake-up is free.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use tracing::{debug, info, warn};
use zbus::blocking::fdo::ObjectManagerProxy;
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::message::Type as MessageType;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{MatchRule, names::OwnedInterfaceName};

use crate::packet::{self, Packet};

/// The name the controller advertises and BlueZ records for it.
pub const DEVICE_NAME: &str = "Daydream controller";

/// The vendor service that carries the reports.
pub const SERVICE_UUID: &str = "0000fe55-0000-1000-8000-00805f9b34fb";

/// The notify characteristic under [`SERVICE_UUID`].
pub const REPORT_UUID: &str = "00000001-1000-1000-8000-00805f9b34fb";

/// How long to wait for BlueZ to finish service discovery after a connect. On the desk it
/// takes well under two seconds for a bonded controller.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(15);

/// Between reconnect attempts while the controller is asleep or out of range.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);

const IFACE_DEVICE         : &str = "org.bluez.Device1";
const IFACE_CHARACTERISTIC : &str = "org.bluez.GattCharacteristic1";
const IFACE_PROPERTIES     : &str = "org.freedesktop.DBus.Properties";

/// Everything that can go wrong before the first report.
#[derive(Debug, thiserror::Error)]
pub enum DaydreamError {
    /// The system bus or BlueZ refused.
    #[error("D-Bus: {0}")]
    Bus(#[from] zbus::Error),

    /// The controller is not paired with any adapter. Pair it once with `bluetoothctl`.
    #[error("no paired device named {DEVICE_NAME:?}{}", match .address { Some(a) => format!(" at {a}"), None => String::new() })]
    NotPaired {
        /// The address that was asked for, if any.
        address : Option<String>,
    },

    /// Connected and resolved, but the report characteristic is missing: not a Daydream.
    #[error("{device} has no characteristic {REPORT_UUID}")]
    NoReportCharacteristic {
        /// The device object.
        device : String,
    },

    /// BlueZ did not finish service discovery in time.
    #[error("{device} did not resolve its services within {RESOLVE_TIMEOUT:?}")]
    ResolveTimeout {
        /// The device object.
        device : String,
    },
}

/// One report and when it arrived.
#[derive(Clone, Copy, Debug)]
pub struct Report {
    /// When the notification reached this process.
    pub at     : Instant,
    /// The decoded report.
    pub packet : Packet,
}

/// A connected controller whose reports are being read.
pub struct Controller {
    /// Where the reports surface. Unbounded, drained by [`reports`](Self::reports).
    reports   : Receiver<Report>,
    /// Set to stop the reader thread.
    stop      : Arc<AtomicBool>,
    /// The bus connection, kept so [`stop`](Self::stop) can send the wake-up.
    conn      : Connection,
    /// The report characteristic's object.
    report    : OwnedObjectPath,
    /// `None` once joined.
    join      : Option<JoinHandle<()>>,
    /// The controller's Bluetooth address, for logging.
    address   : String,
    /// Whether the link is up, as the reader last saw it.
    connected : Arc<AtomicBool>,
}

// --- Controller ---

impl Controller {
    /// Finds the paired controller (the one at `address` if given, else the first one
    /// BlueZ calls [`DEVICE_NAME`]), connects it if it is not connected, subscribes to its
    /// reports and starts the reader thread.
    ///
    /// Connecting blocks for as long as BlueZ takes to find the controller, which is a few
    /// seconds if it is awake and a failure if it is asleep: wake it with the Home button
    /// first.
    pub fn open(address: Option<&str>) -> Result<Controller, DaydreamError> {
        let conn    = Connection::system()?;
        let objects = managed_objects(&conn)?;

        let device = find_device(&objects, address).ok_or_else(|| DaydreamError::NotPaired {
            address : address.map(str::to_owned),
        })?;

        let device_proxy = Proxy::new(&conn, "org.bluez", device.clone(), IFACE_DEVICE)?;
        let address: String = device_proxy.get_property("Address")?;

        ensure_connected(&device_proxy, &device)?;

        // The characteristic objects appear only once services are resolved, so ask again.
        let objects = managed_objects(&conn)?;

        let report = find_report_characteristic(&objects, &device).ok_or_else(|| {
            DaydreamError::NoReportCharacteristic { device: device.to_string() }
        })?;

        let report_proxy = Proxy::new(&conn, "org.bluez", report.clone(), IFACE_CHARACTERISTIC)?;

        // Subscribe to the subtree before StartNotify so the first report is not missed.
        let rule = MatchRule::builder()
            .msg_type(MessageType::Signal)
            .sender("org.bluez")?
            .interface(IFACE_PROPERTIES)?
            .member("PropertiesChanged")?
            .path_namespace(device.clone())?
            .build();

        let signals = MessageIterator::for_match_rule(rule, &conn, None)?;

        report_proxy.call_method("StartNotify", &())?;

        info!(address = %address, device = %device, "daydream controller reporting");

        let stop      = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicBool::new(true));
        let (tx, rx)  = crossbeam_channel::unbounded();

        let join = thread::spawn({
            let stop   = Arc::clone(&stop);
            let reader = Reader {
                device    : device_proxy.to_owned(),
                report    : report_proxy.to_owned(),
                paths     : Paths { device: device, report: report.clone() },
                connected : Arc::clone(&connected),
            };

            move || reader.run(signals, &tx, &stop)
        });

        Ok(Controller {
            reports   : rx,
            stop      : stop,
            conn      : conn,
            report    : report,
            join      : Some(join),
            address   : address,
            connected : connected,
        })
    }

    /// The controller's Bluetooth address.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Whether the link is up: false from a disconnect until the reader has it back.
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Drains the reports queued since the last call, without blocking.
    pub fn reports(&self) -> impl Iterator<Item = Report> + '_ {
        self.reports.try_iter()
    }

    /// Stops notifications and the reader thread. Idempotent.
    ///
    /// Waits for the thread, which returns on the `Notifying` change that `StopNotify`
    /// produces. If the controller is disconnected at the time the thread is inside its
    /// reconnect loop and returns at the next [`RECONNECT_INTERVAL`] instead.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);

        if let Some(join) = self.join.take() {
            if let Ok(proxy) = Proxy::new(&self.conn, "org.bluez", self.report.clone(), IFACE_CHARACTERISTIC) {
                // Fails harmlessly when the controller has already gone away.
                let _ = proxy.call_method("StopNotify", &());
            }

            let _ = join.join();
        }
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.stop();
    }
}

// --- Reader ---

/// The two object paths the reader dispatches on.
struct Paths {
    device : OwnedObjectPath,
    report : OwnedObjectPath,
}

/// The reader thread's state.
struct Reader {
    device    : Proxy<'static>,
    report    : Proxy<'static>,
    paths     : Paths,
    /// Shared with the [`Controller`], so a caller can ask without a report.
    connected : Arc<AtomicBool>,
}

/// The body of `PropertiesChanged`.
type PropertiesChangedBody<'a> = (String, HashMap<&'a str, Value<'a>>, Vec<&'a str>);

impl Reader {
    /// Reads signals until `stop` is set, forwarding decoded reports and riding out
    /// disconnects.
    fn run(self, signals: MessageIterator, tx: &Sender<Report>, stop: &AtomicBool) {
        for message in signals {
            if stop.load(Ordering::Relaxed) {
                return;
            }

            let Ok(message) = message else {
                continue;
            };

            let header = message.header();

            let Some(path) = header.path() else {
                continue;
            };

            let body = message.body();

            let Ok((_iface, changed, _invalidated)) = body.deserialize::<PropertiesChangedBody>() else {
                continue;
            };

            if path.as_str() == self.paths.report.as_str() {
                if let Some(value) = changed.get("Value")
                    && let Ok(bytes) = Vec::<u8>::try_from(value.try_clone().unwrap_or(Value::U8(0)))
                {
                    match packet::decode(&bytes) {
                        Some(packet) => {
                            let _ = tx.send(Report { at: Instant::now(), packet: packet });
                        }

                        None => debug!(len = bytes.len(), "report of unexpected length dropped"),
                    }
                }
            }
            else if path.as_str() == self.paths.device.as_str()
                && let Some(Value::Bool(false)) = changed.get("Connected")
            {
                warn!("daydream controller disconnected; waiting for it to come back");

                self.connected.store(false, Ordering::Relaxed);

                if !self.reconnect(stop) {
                    return;
                }

                self.connected.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Reconnects and resubscribes, retrying every [`RECONNECT_INTERVAL`] until it works
    /// or `stop` is set. Returns whether reading should continue.
    fn reconnect(&self, stop: &AtomicBool) -> bool {
        while !stop.load(Ordering::Relaxed) {
            match self.device.call_method("Connect", &()) {
                Ok(_) => {
                    if let Err(e) = wait_resolved(&self.device) {
                        warn!(error = %e, "daydream controller connected but did not resolve");

                        continue;
                    }

                    match self.report.call_method("StartNotify", &()) {
                        Ok(_) => {
                            info!("daydream controller back");

                            return true;
                        }

                        Err(e) => warn!(error = %e, "StartNotify after reconnect failed"),
                    }
                }

                Err(e) => debug!(error = %e, "daydream controller still away"),
            }

            thread::sleep(RECONNECT_INTERVAL);
        }

        false
    }
}

// --- BlueZ object lookups ---

/// Every object BlueZ exports, with its interfaces and their properties.
type Objects = HashMap<OwnedObjectPath, HashMap<OwnedInterfaceName, HashMap<String, OwnedValue>>>;

fn managed_objects(conn: &Connection) -> Result<Objects, zbus::Error> {
    let manager = ObjectManagerProxy::builder(conn)
        .destination("org.bluez")?
        .path("/")?
        .build()?;

    Ok(manager.get_managed_objects()?)
}

/// The device object for `address`, or for the first device named [`DEVICE_NAME`].
fn find_device(objects: &Objects, address: Option<&str>) -> Option<OwnedObjectPath> {
    let mut candidates: Vec<_> = objects
        .iter()
        .filter_map(|(path, ifaces)| {
            let props = ifaces.get(IFACE_DEVICE)?;

            let matches = match address {
                Some(a) => string_prop(props, "Address").is_some_and(|x| x.eq_ignore_ascii_case(a)),
                None    => string_prop(props, "Name").is_some_and(|n| n == DEVICE_NAME),
            };

            matches.then(|| path.clone())
        })
        .collect();

    // Deterministic when two adapters both know the controller.
    candidates.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    candidates.into_iter().next()
}

/// The report characteristic under `device`.
fn find_report_characteristic(objects: &Objects, device: &OwnedObjectPath) -> Option<OwnedObjectPath> {
    let prefix = format!("{}/", device.as_str());

    objects
        .iter()
        .filter(|(path, _)| path.as_str().starts_with(&prefix))
        .find_map(|(path, ifaces)| {
            let props = ifaces.get(IFACE_CHARACTERISTIC)?;

            (string_prop(props, "UUID") == Some(REPORT_UUID)).then(|| path.clone())
        })
}

fn string_prop<'a>(props: &'a HashMap<String, OwnedValue>, name: &str) -> Option<&'a str> {
    props.get(name)?.downcast_ref::<&str>().ok()
}

/// Connects `device` if it is not connected and waits for its services to resolve.
fn ensure_connected(device: &Proxy<'_>, path: &OwnedObjectPath) -> Result<(), DaydreamError> {
    let connected: bool = device.get_property("Connected")?;

    if !connected {
        info!(device = %path, "connecting the daydream controller");

        device.call_method("Connect", &())?;
    }

    wait_resolved(device)
}

/// Polls `ServicesResolved` until it is true or [`RESOLVE_TIMEOUT`] passes.
fn wait_resolved(device: &Proxy<'_>) -> Result<(), DaydreamError> {
    let start = Instant::now();

    loop {
        let resolved: bool = device.get_property("ServicesResolved")?;

        if resolved {
            return Ok(());
        }

        if start.elapsed() > RESOLVE_TIMEOUT {
            return Err(DaydreamError::ResolveTimeout { device: device.path().to_string() });
        }

        thread::sleep(Duration::from_millis(100));
    }
}
