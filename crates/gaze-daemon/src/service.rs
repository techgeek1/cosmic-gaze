//! The control interface, served on the session bus.
//!
//! One object at [`BUS_PATH`] on the well-known name [`BUS_NAME`]. Every property reads
//! the session's latest [`Status`] out of the shared [`Live`]; every method sets a flag
//! the session loop polls at its next sample. Nothing here blocks on the session, so a
//! `busctl` call answers whether or not a tracker is on the bus.

use std::sync::Arc;

use gaze_config::{BUS_NAME, BUS_PATH};
use gaze_proto::Live;
use tracing::info;
use zbus::blocking::Connection;
use zbus::blocking::connection::Builder;

/// The interface implementation. Holds the session's shared state and nothing else.
pub struct Service {
    live : Arc<Live>,
}

// --- Service ---

impl Service {
    /// Claims the bus name and serves the interface. The connection runs on zbus's own
    /// thread; dropping it drops the name.
    pub fn serve(live: Arc<Live>) -> zbus::Result<Connection> {
        let conn = Builder::session()?
            .name(BUS_NAME)?
            .serve_at(BUS_PATH, Service { live: live })?
            .build()?;

        info!(name = BUS_NAME, path = BUS_PATH, "control interface up");

        Ok(conn)
    }
}

#[zbus::interface(name = "dev.techgeek1.CosmicGaze")]
impl Service {
    /// Stops drawing and injecting until `Resume`. The tracker keeps streaming.
    fn pause(&self) {
        info!("pause requested");
        self.live.set_paused(true);
    }

    /// Undoes `Pause`.
    fn resume(&self) {
        info!("resume requested");
        self.live.set_paused(false);
    }

    /// Forgets the day's offset: every anchor, back to the calibration alone.
    fn reset_offset(&self) {
        info!("offset reset requested");
        self.live.request_reset_offset();
    }

    /// Ends the session and exits, as a signal would.
    fn quit(&self) {
        info!("quit requested");
        self.live.stop();
    }

    #[zbus(property)]
    fn tracker(&self) -> bool {
        self.live.status().tracker
    }

    #[zbus(property)]
    fn calibrated(&self) -> bool {
        self.live.status().calibrated
    }

    #[zbus(property)]
    fn controller(&self) -> bool {
        self.live.status().controller
    }

    #[zbus(property)]
    fn paused(&self) -> bool {
        self.live.paused()
    }

    #[zbus(property)]
    fn mode(&self) -> String {
        self.live.status().mode.as_str().to_string()
    }

    #[zbus(property)]
    fn offset_updates(&self) -> u64 {
        self.live.status().offset_updates
    }

    #[zbus(property)]
    fn offset_jumps(&self) -> u64 {
        self.live.status().offset_jumps
    }

    #[zbus(property)]
    fn offset_yaw_deg(&self) -> f64 {
        self.live.status().offset_yaw_deg
    }

    #[zbus(property)]
    fn offset_pitch_deg(&self) -> f64 {
        self.live.status().offset_pitch_deg
    }
}
