//! Bulk USB transport for the ET5 via rusb/libusb: find the device, open the vendor
//! session, and move already-framed bytes (see `crate::ttp`) in both directions.
//!
//! Concurrency: libusb's synchronous API is thread safe per endpoint, so one thread can
//! sit in `recv` while another calls `send`. `crate::device` shares a `Transport`
//! between its reader thread and the request path through an `Arc`.

use std::time::Duration;

use rusb::{DeviceHandle, GlobalContext};

// --- USB identity and endpoints ---

/// Tobii vendor id.
pub const VID: u16 = 0x2104;

/// ET5 runtime-firmware product id.
pub const PID_RUNTIME: u16 = 0x0313;

/// Runtime product id used by some IS5 units.
pub const PID_RUNTIME_ALT: u16 = 0x031e;

/// Bulk IN endpoint address.
const EP_IN: u8 = 0x83;

/// Bulk OUT endpoint address.
const EP_OUT: u8 = 0x05;

/// The single vendor interface.
const INTERFACE: u8 = 0;

/// Vendor control request that opens the TTP session. Required before any bulk traffic.
const CTRL_SESSION_OPEN: u8 = 0x41;

/// Vendor control request that closes the session.
const CTRL_SESSION_CLOSE: u8 = 0x42;

/// bmRequestType for the session controls: host-to-device, vendor, interface recipient.
const CTRL_REQUEST_TYPE: u8 = 0x41;

/// Maximum bytes per OUT transfer. Larger frames are split.
const OUT_CHUNK: usize = 8192;

/// Payload bytes per continuation transfer (chunk minus its 8-byte envelope).
const OUT_CONT_DATA: usize = OUT_CHUNK - 8;

/// Read size for IN transfers. Large enough for any single gaze notification burst.
pub const IN_CHUNK: usize = 16384;

/// Timeout for OUT transfers and the session controls.
const SEND_TIMEOUT: Duration = Duration::from_millis(2000);

// --- Transport ---

/// An open USB session to the tracker: interface claimed, vendor session opened.
/// Dropping it closes the session and releases the interface.
pub struct Transport {
    handle: DeviceHandle<GlobalContext>,
}

impl Transport {
    /// Opens the first connected runtime-mode ET5. Fails with a description a user can
    /// act on when the device is absent, in bootloader mode, or not accessible.
    pub fn open() -> Result<Self, TransportError> {
        let handle = rusb::open_device_with_vid_pid(VID, PID_RUNTIME)
            .or_else(|| rusb::open_device_with_vid_pid(VID, PID_RUNTIME_ALT))
            .ok_or(TransportError::NotFound)?;

        // The kernel has no driver for the vendor interface in practice, but detaching
        // defensively costs nothing and covers a future usbhid grab.
        if handle.kernel_driver_active(INTERFACE).unwrap_or(false) {
            let _ = handle.detach_kernel_driver(INTERFACE);
        }

        handle.claim_interface(INTERFACE).map_err(TransportError::Claim)?;

        let opened = handle.write_control(
            CTRL_REQUEST_TYPE,
            CTRL_SESSION_OPEN,
            0,
            0,
            &[],
            SEND_TIMEOUT,
        );

        if let Err(e) = opened {
            let _ = handle.release_interface(INTERFACE);

            return Err(TransportError::SessionOpen(e));
        }

        Ok(Self { handle: handle })
    }

    /// The tracker's position on the bus, `(bus number, device address)`.
    ///
    /// libusb hands out a new address every time a device enumerates, so this pair
    /// changing across a reconnect is proof the firmware rebooted rather than the host
    /// merely reopening the handle. That distinction matters because an ET5 reboot
    /// resets the on-device eye model to the factory blob, silently discarding a
    /// retrain that had just finished.
    pub fn usb_address(&self) -> (u8, u8) {
        let device = self.handle.device();

        (device.bus_number(), device.address())
    }

    /// Sends one already-enveloped frame, splitting it across transfers when it exceeds
    /// the device's 8 KB transfer size.
    ///
    /// The first transfer of a split frame must carry the per-transfer data length in
    /// its envelope, not the full TTP length `crate::ttp::envelope_out` wrote there;
    /// the device learns the total from the TTP header's `plen` instead. Continuation
    /// transfers get their own `[0; 4][len: u32 LE]` envelope.
    pub fn send(&self, bytes: &[u8]) -> Result<(), TransportError> {
        if bytes.len() <= OUT_CHUNK {
            self.write_all(bytes)?;

            return Ok(());
        }

        // First chunk with the envelope length patched to this transfer's data size.
        let mut first = bytes[..OUT_CHUNK].to_vec();
        first[4..8].copy_from_slice(&(OUT_CONT_DATA as u32).to_le_bytes());
        self.write_all(&first)?;

        // Continuation chunks, each with a fresh envelope.
        let mut offset = OUT_CHUNK;

        while offset < bytes.len() {
            let end = (offset + OUT_CONT_DATA).min(bytes.len());

            let mut chunk = Vec::with_capacity(8 + (end - offset));
            chunk.extend_from_slice(&[0u8; 4]);
            chunk.extend_from_slice(&((end - offset) as u32).to_le_bytes());
            chunk.extend_from_slice(&bytes[offset..end]);
            self.write_all(&chunk)?;

            offset = end;
        }

        Ok(())
    }

    /// Reads one IN transfer into `buf`. `Ok(None)` on timeout, which is the normal
    /// idle case and how a reader loop stays responsive to shutdown.
    pub fn recv(&self, buf: &mut [u8], timeout: Duration)
        -> Result<Option<usize>, TransportError>
    {
        match self.handle.read_bulk(EP_IN, buf, timeout) {
            Ok(n)                     => {
                // Transfer-boundary visibility for protocol debugging.
                tracing::trace!(
                    "IN {n:5} bytes: {}",
                    buf[..n.min(16)].iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "),
                );

                // Ground-truth capture: append [len: u32 LE][bytes] per read to the
                // file named by GAZE_ET5_RAW_DUMP. Debug aid only.
                if let Ok(path) = std::env::var("GAZE_ET5_RAW_DUMP") {
                    use std::io::Write;

                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true).append(true).open(&path)
                    {
                        let _ = f.write_all(&(n as u32).to_le_bytes());
                        let _ = f.write_all(&buf[..n]);
                    }
                }

                Ok(Some(n))
            }
            Err(rusb::Error::Timeout) => Ok(None),
            Err(e)                    => Err(TransportError::Io(e)),
        }
    }
}

impl Transport {
    /// Writes one transfer and verifies the full length went out.
    fn write_all(&self, bytes: &[u8]) -> Result<(), TransportError> {
        let n = self.handle.write_bulk(EP_OUT, bytes, SEND_TIMEOUT)
            .map_err(TransportError::Io)?;

        if n != bytes.len() {
            return Err(TransportError::ShortWrite { wrote: n, want: bytes.len() });
        }

        Ok(())
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        // Best effort: the device closes the session itself on disconnect.
        let _ = self.handle.write_control(
            CTRL_REQUEST_TYPE,
            CTRL_SESSION_CLOSE,
            0,
            0,
            &[],
            Duration::from_millis(500),
        );
        let _ = self.handle.release_interface(INTERFACE);
    }
}

// --- Errors ---

/// USB transport failure.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error(
        "no ET5 in runtime mode on the bus (want {VID:04x}:{PID_RUNTIME:04x}); \
         check `lsusb -d 2104:` and the udev rule"
    )]
    NotFound,
    #[error("could not claim interface 0 (device busy? another client running?): {0}")]
    Claim(#[source] rusb::Error),
    #[error("vendor session open (ctrl 0x41) failed: {0}")]
    SessionOpen(#[source] rusb::Error),
    #[error("bulk transfer failed: {0}")]
    Io(#[source] rusb::Error),
    #[error("short bulk write: {wrote} of {want} bytes")]
    ShortWrite { wrote: usize, want: usize },
}
