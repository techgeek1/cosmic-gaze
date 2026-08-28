//! The tracker feed: raw device frames into a ring the collector reads backward from.
//!
//! The provider owns the device and the reconnect story, including re-uploading and
//! verifying the eye model on every connect; all this adds is a thread that keeps
//! asking it for frames and a ring buffer holding the last few seconds. A click looks
//! backward through that ring, which is why nothing here decides anything: the frames
//! go in stamped with the collector's clock and the collector picks its own window.
//!
//! The one thing this thread does watch for is the link coming back with a *different*
//! eye model, which can only happen if the user retrained while the collector was
//! running. Every client-side row is keyed to the blob, so that has to start a new
//! session file rather than quietly mix two feature extractors into one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};
use gaze_provider_et5::{BlobReport, DisplayArea, Et5Provider};
use gaze_provider_et5::sweep::TimedFrame;
use tracing::{info, warn};

use crate::frames::GazeRing;

/// How long the reader waits on the device before looping to check the stop flag.
const FRAME_TIMEOUT: Duration = Duration::from_millis(100);

/// How much device history the ring holds, seconds. The gaze window a click reads is
/// 1.6 s wide and the click is written a moment after its trailing edge, so three
/// seconds covers it with room for a slow recognition pass.
pub const RING_SPAN_S: f64 = 3.0;

/// Something the collector has to react to rather than just record.
#[derive(Clone, Debug)]
pub enum TrackerEvent {
    /// The link dropped and came back. The report is the blob the device holds now;
    /// the collector compares it with the session's own and rotates the file when they
    /// differ.
    Reconnected { report : BlobReport },
}

/// A running device feed.
pub struct TrackerFeed {
    ring    : Arc<Mutex<GazeRing>>,
    events  : Receiver<TrackerEvent>,
    stop    : Arc<AtomicBool>,
    join    : Option<JoinHandle<()>>,
    /// The blob the device held when the feed started.
    blob    : BlobReport,
    /// The plane declared on the device, which is what the firmware's 2D output means.
    area    : DisplayArea,
}

// --- TrackerFeed ---

impl TrackerFeed {
    /// Reads the device's identity, then hands the provider to a reader thread.
    ///
    /// `t0` is the collector's clock. Frames are stamped with their arrival time
    /// against it, exactly as `record` stamps them, so a click's window and a session's
    /// frames are on one clock.
    pub fn start(mut provider: Et5Provider, t0: Instant) -> Result<TrackerFeed> {
        let blob = provider.device_blob_report()
            .context("retrieving the on-device calibration blob")?;

        let area = provider.device_display_area()
            .context("reading the declared display area back off the device")?;

        info!(blob = %blob.short(), bytes = blob.len, "tracker feed started");

        let ring     = Arc::new(Mutex::new(GazeRing::new(RING_SPAN_S)));
        let stop     = Arc::new(AtomicBool::new(false));
        let (tx, rx) = crossbeam_channel::unbounded();

        let join = thread::Builder::new()
            .name("gaze-clicks-tracker".to_string())
            .spawn({
                let ring = Arc::clone(&ring);
                let stop = Arc::clone(&stop);

                move || run(provider, &ring, &tx, &stop, t0)
            })
            .context("spawning the tracker reader thread")?;

        Ok(TrackerFeed {
            ring   : ring,
            events : rx,
            stop   : stop,
            join   : Some(join),
            blob   : blob,
            area   : area,
        })
    }

    /// Every frame in `[t0_s, t1_s]`, oldest first.
    pub fn window(&self, t0_s: f64, t1_s: f64) -> Vec<TimedFrame> {
        self.ring
            .lock()
            .map(|ring| ring.window(t0_s, t1_s))
            .unwrap_or_default()
    }

    /// Where reconnect notices arrive.
    pub fn events(&self) -> &Receiver<TrackerEvent> {
        &self.events
    }

    /// The blob the device held at the start of the feed.
    pub fn blob(&self) -> &BlobReport {
        &self.blob
    }

    /// The plane declared on the device.
    pub fn area(&self) -> DisplayArea {
        self.area
    }

    /// Stops the reader thread and closes the device. Idempotent.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);

        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for TrackerFeed {
    fn drop(&mut self) {
        self.stop();
    }
}

// --- Thread body ---

/// Pumps frames into the ring until the stop flag is set.
fn run(
    mut provider : Et5Provider,
    ring         : &Mutex<GazeRing>,
    events       : &Sender<TrackerEvent>,
    stop         : &AtomicBool,
    t0           : Instant,
)
{
    let mut connects = provider.connects();

    while !stop.load(Ordering::Relaxed) {
        if let Some(frame) = provider.next_frame(FRAME_TIMEOUT) {
            let timed = TimedFrame {
                t_s   : t0.elapsed().as_secs_f64(),
                frame : frame,
            };

            if let Ok(mut ring) = ring.lock() {
                ring.push(timed);
            }
        }

        // A reconnect re-uploads the host's blob, so the model is normally the same one
        // as before; the retrieve is here for the case where it is not.
        if provider.connects() != connects {
            connects = provider.connects();

            match provider.device_blob_report() {
                Some(report) => {
                    let _ = events.send(TrackerEvent::Reconnected { report: report });
                }
                None         => warn!("reconnected but could not retrieve the blob"),
            }
        }
    }

    // Dropping the provider joins the device's reader thread and releases the USB
    // interface, which the next process to open the tracker needs.
    drop(provider);
}
