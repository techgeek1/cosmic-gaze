//! The ET5 wire protocol: TTP frames, the USB transfer envelope, TLV payloads, and the
//! inbound reassembly buffer. Byte-level only, no I/O. `crate::transport` moves these
//! bytes over USB and `crate::device` gives them meaning.
//!
//! Wire shapes (reverse engineered; provenance is the tobiifree project, confirmed
//! against this unit):
//!
//! - Outbound USB transfer: `[0x00, 0, 0, 0][ttp_len: u32 LE][TTP frame]`. The length
//!   excludes the 8-byte envelope itself.
//! - Inbound USB transfer: the same envelope with direction byte `0x01`.
//! - TTP frame: a 24-byte big-endian header `[magic][seq][0][op][0][plen]` followed by
//!   `plen` payload bytes.
//! - Payloads are TLV fields `[type: u8][size: u32 BE][body]`. Request payloads start
//!   with a two-byte `[0x00, 0x00]` prefix (except the bare-u32 settings ops), and
//!   response payloads carry the same prefix.
//! - Scalars are fixed point: Q42 (`i64 / 2^42`) for millimetres and normalised
//!   coordinates, Q16 (`i32 / 2^16`) for small scalars like pupil diameter.

// --- Sizes and magics ---

/// TTP header length in bytes.
pub const TTP_HDR_SIZE: usize = 24;

/// USB transfer envelope length in bytes.
pub const ENVELOPE_SIZE: usize = 8;

/// Frame magic for host requests.
pub const MAGIC_REQ: u32 = 0x51;

/// Frame magic for device responses (seq echoes the request).
pub const MAGIC_RSP: u32 = 0x52;

/// Frame magic for unsolicited notifications (gaze stream).
pub const MAGIC_NOTIFY: u32 = 0x53;

/// Stream id of the gaze notification stream.
pub const STREAM_GAZE: u32 = 0x500;

/// Upper bound on a reassembled frame payload. The calibration blob response grows
/// with the point count (751 KB measured after a 12-point calibration); sized with
/// generous headroom because rejecting a legitimate frame desynchronises the stream.
pub const FRAME_MAX: usize = 8 * 1024 * 1024;

// --- Operation codes ---

/// Session hello, first frame after the USB session opens.
pub const OP_HELLO: u32 = 0x3e8;

/// Subscribe to a notification stream.
pub const OP_SUBSCRIBE: u32 = 0x4c4;

/// Declare the display plane the device projects 2D gaze onto.
pub const OP_SET_DISPLAY_AREA: u32 = 0x5a0;

/// Read back the declared display plane.
pub const OP_GET_DISPLAY_AREA: u32 = 0x596;

/// Open a calibration session. Without it the device acks and discards added points.
pub const OP_CAL_START: u32 = 0x3f2;

/// Close the calibration session.
pub const OP_CAL_STOP: u32 = 0x3fc;

/// Add a calibration point at normalised display coordinates. The device collects its
/// own raw samples while this request is in flight.
pub const OP_CAL_POINT_ADD2D: u32 = 0x406;

/// Drop previously collected calibration points.
pub const OP_CAL_CLEAR: u32 = 0x424;

/// Fit and commit the eye model from the collected points.
pub const OP_CAL_POINTS_APPLY: u32 = 0x42e;

/// Ask the device which calibration point it would like next. Exploratory: the
/// opcode is named in Talon's map but nothing there or here sends it, so the reply's
/// shape is a guess until a device answers one.
pub const OP_CAL_POINT_SUGGESTION: u32 = 0x442;

/// Download the opaque on-device calibration blob.
pub const OP_CAL_RETRIEVE: u32 = 0x44c;

/// Upload a previously downloaded calibration blob.
pub const OP_CAL_APPLY: u32 = 0x456;

/// Read the stream rates: a pair of u32, camera frames per second and gaze frames per
/// second (`(132, 33)` on the ET5). Read-only; nothing found sets it, and the ET5's
/// 33 Hz gaze is its specification, not a mode.
pub const OP_GET_FREQUENCIES: u32 = 0x672;

/// Query which realm (privilege domain) guards the calibration ops.
pub const OP_QUERY_REALM: u32 = 0x640;

/// Open a realm; the response carries the HMAC challenge.
pub const OP_OPEN_REALM: u32 = 0x76c;

/// Answer the realm challenge with an HMAC-MD5 digest.
pub const OP_REALM_RESPONSE: u32 = 0x776;

/// Close an opened realm.
pub const OP_CLOSE_REALM: u32 = 0x77b;

/// Select which eyes are tracked (bare-u32 payload; 3 = both).
pub const OP_ENABLED_EYE_SET: u32 = 0x0c58;

/// Pause or unpause tracking (bare-u32 payload; 0 = running).
pub const OP_PAUSE_SET: u32 = 0x0c1c;

// --- Fixed-point codecs ---

/// Scale of the Q42 fixed-point format (2^42).
const Q42_SCALE: f64 = 4398046511104.0;

/// Encodes a value into Q42 fixed point.
pub fn q42_encode(v: f64) -> i64 {
    (v * Q42_SCALE).round() as i64
}

/// Decodes a Q42 fixed-point value.
pub fn q42_decode(raw: i64) -> f64 {
    raw as f64 / Q42_SCALE
}

// --- TLV struct tags ---

/// Prolog tag of a 3D point (three Q42 values).
const TAG_POINT3: u32 = 0x31f41;

/// Prolog tag of a 2D point (two Q42 values).
const TAG_POINT2: u32 = 0x21f40;

/// Trailer tag observed at the end of every set_display_area capture.
const TAG_DISPLAY_TRAILER: u32 = 0x10100;

/// Trailer value observed alongside `TAG_DISPLAY_TRAILER` (decimal 12345; meaning
/// unknown, replicated verbatim from captures).
const DISPLAY_TRAILER_VALUE: u32 = 0x3039;

// --- TLV writers ---

/// Appends a prolog tag field: `[5][size=4][tag]`.
fn put_tag(out: &mut Vec<u8>, tag: u32) {
    out.push(5);
    out.extend_from_slice(&4u32.to_be_bytes());
    out.extend_from_slice(&tag.to_be_bytes());
}

/// Appends a u32 field: `[2][size=4][v]`.
fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.push(2);
    out.extend_from_slice(&4u32.to_be_bytes());
    out.extend_from_slice(&v.to_be_bytes());
}

/// Appends a Q42 field: `[4][size=8][round(v * 2^42)]`.
fn put_q42(out: &mut Vec<u8>, v: f64) {
    out.push(4);
    out.extend_from_slice(&8u32.to_be_bytes());
    out.extend_from_slice(&q42_encode(v).to_be_bytes());
}

/// Appends a 3D point: prolog tag then three Q42 values.
fn put_point3(out: &mut Vec<u8>, x: f64, y: f64, z: f64) {
    put_tag(out, TAG_POINT3);
    put_q42(out, x);
    put_q42(out, y);
    put_q42(out, z);
}

// --- Frame building ---

/// Builds a bare TTP frame: 24-byte header plus payload, no USB envelope.
pub fn frame(seq: u32, op: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(TTP_HDR_SIZE + payload.len());
    out.extend_from_slice(&MAGIC_REQ.to_be_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&op.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);

    out
}

/// Wraps a TTP frame in the outbound USB envelope.
pub fn envelope_out(ttp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ENVELOPE_SIZE + ttp.len());
    out.extend_from_slice(&[0x00, 0, 0, 0]);
    out.extend_from_slice(&(ttp.len() as u32).to_le_bytes());
    out.extend_from_slice(ttp);

    out
}

/// Builds an enveloped request frame for an arbitrary opcode and payload. The probe
/// path for opcodes nothing here understands yet; production callers use the typed
/// builders below.
pub fn raw_command(seq: u32, op: u32, payload: &[u8]) -> Vec<u8> {
    command(seq, op, payload)
}

/// Builds an enveloped request frame in one step.
fn command(seq: u32, op: u32, payload: &[u8]) -> Vec<u8> {
    envelope_out(&frame(seq, op, payload))
}

/// Builds an enveloped request whose payload is just the `[0x00, 0x00]` prefix.
fn empty_command(seq: u32, op: u32) -> Vec<u8> {
    command(seq, op, &[0x00, 0x00])
}

// --- Request builders ---

/// Payload of the hello request, replicated byte for byte from USB captures. Encodes a
/// capability/version list the device expects but whose fields are not understood.
const HELLO_PAYLOAD: [u8; 47] = [
    0x00, 0x00, 0x17, 0x00, 0x00, 0x00, 0x28, 0x00, 0x00, 0x00, 0x09,
    0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x02,
    0x00, 0x01, 0x00, 0x03, 0x00, 0x01, 0x00, 0x04, 0x00, 0x01, 0x00, 0x05,
    0x00, 0x01, 0x00, 0x06, 0x00, 0x01, 0x00, 0x07, 0x00, 0x01, 0x00, 0x08,
];

/// Builds the hello request that opens the TTP session.
pub fn hello(seq: u32) -> Vec<u8> {
    command(seq, OP_HELLO, &HELLO_PAYLOAD)
}

/// Builds a stream subscription. The 20-byte payload is a captured template with the
/// stream id patched in at bytes 9..11 (big endian).
pub fn subscribe(seq: u32, stream_id: u16) -> Vec<u8> {
    let mut payload: [u8; 20] = [
        0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00,
        0x00, 0x17, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00,
    ];
    payload[9]  = (stream_id >> 8) as u8;
    payload[10] = stream_id as u8;

    command(seq, OP_SUBSCRIBE, &payload)
}

/// An axis-aligned display plane in tracker space, millimetres. `ox_mm`/`oy_mm` locate
/// the bottom-left corner; `z_mm` is the plane depth (positive toward the user).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplayRect {
    pub w_mm  : f64,
    pub h_mm  : f64,
    pub ox_mm : f64,
    pub oy_mm : f64,
    pub z_mm  : f64,
}

/// The display plane as the device reports it: three corners in tracker space (mm).
/// The fourth corner is implied.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DisplayArea {
    pub tl_mm : [f64; 3],
    pub tr_mm : [f64; 3],
    pub bl_mm : [f64; 3],
}

impl DisplayArea {
    /// The three corners of an axis-aligned plane. The wire format only speaks
    /// corners, so anything that declares a `DisplayRect` goes through here.
    pub fn from_rect(rect: DisplayRect) -> Self {
        let x0 = rect.ox_mm;
        let x1 = rect.ox_mm + rect.w_mm;
        let y0 = rect.oy_mm;
        let y1 = rect.oy_mm + rect.h_mm;

        Self {
            tl_mm : [x0, y1, rect.z_mm],
            tr_mm : [x1, y1, rect.z_mm],
            bl_mm : [x0, y0, rect.z_mm],
        }
    }
}

/// Builds set_display_area from an axis-aligned rect.
pub fn set_display_area(seq: u32, rect: DisplayRect) -> Vec<u8> {
    set_display_area_corners(seq, DisplayArea::from_rect(rect))
}

/// Builds set_display_area from explicit corners. Only TL/TR/BL go on the wire.
pub fn set_display_area_corners(seq: u32, area: DisplayArea) -> Vec<u8> {
    let mut payload = vec![0x00, 0x00];
    put_point3(&mut payload, area.tl_mm[0], area.tl_mm[1], area.tl_mm[2]);
    put_point3(&mut payload, area.tr_mm[0], area.tr_mm[1], area.tr_mm[2]);
    put_point3(&mut payload, area.bl_mm[0], area.bl_mm[1], area.bl_mm[2]);
    put_tag(&mut payload, TAG_DISPLAY_TRAILER);
    put_u32(&mut payload, DISPLAY_TRAILER_VALUE);

    command(seq, OP_SET_DISPLAY_AREA, &payload)
}

/// Builds get_display_area (empty payload, not even the two-byte prefix).
pub fn get_display_area(seq: u32) -> Vec<u8> {
    command(seq, OP_GET_DISPLAY_AREA, &[])
}

/// Decodes a get_display_area response payload into the three corners.
pub fn decode_display_area(payload: &[u8]) -> Option<DisplayArea> {
    if payload.len() < 2 {
        return None;
    }

    let mut r = TlvReader::new(payload);
    r.pos = 2;

    let tl = r.read_point3().ok()?;
    let tr = r.read_point3().ok()?;
    let bl = r.read_point3().ok()?;

    Some(DisplayArea { tl_mm: tl, tr_mm: tr, bl_mm: bl })
}

/// Builds get_frequencies.
pub fn get_frequencies(seq: u32) -> Vec<u8> {
    command(seq, OP_GET_FREQUENCIES, &[])
}

/// Decodes a get_frequencies response: prolog tag, then camera and gaze rates.
pub fn decode_frequencies(payload: &[u8]) -> Option<(u32, u32)> {
    if payload.len() < 2 {
        return None;
    }

    let mut r = TlvReader::new(payload);
    r.pos = 2;

    r.read_prolog_tag().ok()?;

    let camera = r.read_u32().ok()?;
    let gaze   = r.read_u32().ok()?;

    Some((camera, gaze))
}

/// Builds query_realm.
pub fn query_realm(seq: u32) -> Vec<u8> {
    empty_command(seq, OP_QUERY_REALM)
}

/// Builds open_realm for the given realm type. The trailing raw zero byte is a "choice"
/// field replicated from captures.
pub fn open_realm(seq: u32, realm_type: u32) -> Vec<u8> {
    let mut payload = vec![0x00, 0x00];
    put_u32(&mut payload, realm_type);
    payload.push(0x00);

    command(seq, OP_OPEN_REALM, &payload)
}

/// Builds realm_response carrying the 16-byte HMAC-MD5 digest of the challenge.
pub fn realm_response(seq: u32, realm_id: u32, field_210: u32, digest: &[u8; 16]) -> Vec<u8> {
    let mut payload = vec![0x00, 0x00];
    put_u32(&mut payload, realm_id);
    put_u32(&mut payload, field_210);
    payload.extend_from_slice(digest);

    command(seq, OP_REALM_RESPONSE, &payload)
}

/// Builds close_realm.
pub fn close_realm(seq: u32, realm_id: u32) -> Vec<u8> {
    let mut payload = vec![0x00, 0x00];
    put_u32(&mut payload, realm_id);

    command(seq, OP_CLOSE_REALM, &payload)
}

/// Builds cal_start.
pub fn cal_start(seq: u32) -> Vec<u8> {
    empty_command(seq, OP_CAL_START)
}

/// Builds cal_stop.
pub fn cal_stop(seq: u32) -> Vec<u8> {
    empty_command(seq, OP_CAL_STOP)
}

/// Builds cal_clear.
pub fn cal_clear(seq: u32) -> Vec<u8> {
    empty_command(seq, OP_CAL_CLEAR)
}

/// Builds cal_points_apply.
pub fn cal_points_apply(seq: u32) -> Vec<u8> {
    empty_command(seq, OP_CAL_POINTS_APPLY)
}

/// Builds cal_point_add2d at normalised display coordinates. `eye_mask`: 1 left,
/// 2 right, 3 both.
pub fn cal_point_add2d(seq: u32, x: f64, y: f64, eye_mask: u32) -> Vec<u8> {
    let mut payload = vec![0x00, 0x00];
    put_q42(&mut payload, x);
    put_q42(&mut payload, y);
    put_u32(&mut payload, eye_mask);

    command(seq, OP_CAL_POINT_ADD2D, &payload)
}

/// Builds cal_point_suggestion. Empty payload, like the other bare calibration ops.
pub fn cal_point_suggestion(seq: u32) -> Vec<u8> {
    empty_command(seq, OP_CAL_POINT_SUGGESTION)
}

/// Best-effort decode of a point-suggestion response as normalised 2D points.
///
/// The reply's layout is unknown, so this reads what the rest of the protocol would
/// put there — a run of `TAG_POINT2` prologs, or bare Q42 pairs — and stops at the
/// first field it does not recognise. An empty result means "not that shape"; the
/// caller is expected to log the raw bytes either way and depend on neither.
pub fn decode_point_suggestion(payload: &[u8]) -> Vec<[f64; 2]> {
    if payload.len() < 2 {
        return Vec::new();
    }

    let mut r      = TlvReader::new(payload);
    let mut points = Vec::new();
    r.pos = 2;

    while r.remaining() > 0 {
        // A prolog'd point first; failing that, two loose Q42 values, which is how
        // cal_point_add2d carries the same information in the other direction.
        let before = r.pos;

        if let Ok(p) = r.read_point2() {
            points.push(p);

            continue;
        }

        r.pos = before;

        let (Ok(x), Ok(y)) = (r.read_q42(), r.read_q42()) else {
            break;
        };

        points.push([x, y]);
    }

    points
}

/// Builds cal_retrieve.
pub fn cal_retrieve(seq: u32) -> Vec<u8> {
    empty_command(seq, OP_CAL_RETRIEVE)
}

/// Builds cal_apply carrying an opaque calibration blob (as returned by
/// `crate::device::Device::cal_finish`, without the response's status prefix).
pub fn cal_apply(seq: u32, blob: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(2 + blob.len());
    payload.extend_from_slice(&[0x00, 0x00]);
    payload.extend_from_slice(blob);

    command(seq, OP_CAL_APPLY, &payload)
}

/// Builds a bare-u32 settings request (ENABLED_EYE_SET, PAUSE_SET). Unlike every other
/// request these payloads carry no `[0x00, 0x00]` prefix.
pub fn set_u32(seq: u32, op: u32, value: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(9);
    put_u32(&mut payload, value);

    command(seq, op, &payload)
}

// --- Realm response scanning ---

// The realm responses use field headers this scanner walks 4 bytes at a time with a
// 16-bit size at offset 2, which is how the reference implementation reads them. It is
// a heuristic scan rather than a strict TLV parse, kept verbatim because it is the
// variant proven against hardware.

/// Returns the `index`-th 4-byte field value in a realm response payload, or 0.
pub fn realm_u32_at(payload: &[u8], index: usize) -> u32 {
    let mut pos   = 2usize;
    let mut found = 0usize;

    while pos + 4 <= payload.len() {
        let size = u16::from_be_bytes([payload[pos + 2], payload[pos + 3]]) as usize;
        pos += 4;

        if size == 4 && pos + 4 <= payload.len() {
            if found == index {
                let b = [payload[pos], payload[pos + 1], payload[pos + 2], payload[pos + 3]];

                return u32::from_be_bytes(b);
            }

            found += 1;
        }

        pos += size;
    }

    0
}

/// Returns the first field longer than 4 bytes in a realm response payload: the HMAC
/// challenge in an open_realm response.
pub fn realm_challenge(payload: &[u8]) -> Option<&[u8]> {
    let mut pos = 2usize;

    while pos + 4 <= payload.len() {
        let size = u16::from_be_bytes([payload[pos + 2], payload[pos + 3]]) as usize;
        pos += 4;

        if size > 4 && pos + size <= payload.len() {
            return Some(&payload[pos..pos + size]);
        }

        pos += size;
    }

    None
}

// --- TLV reader ---

/// Sequential reader over a TLV payload. `pos` is public so callers can skip known
/// prefixes (every response payload starts with a two-byte status).
pub struct TlvReader<'a> {
    buf     : &'a [u8],
    pub pos : usize,
}

impl<'a> TlvReader<'a> {
    /// Creates a reader at the start of `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf: buf, pos: 0 }
    }

    /// Bytes left to read.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Reads a field header `[type][size]` without consuming it.
    pub fn peek_header(&self) -> Result<(u8, u32), TlvError> {
        if self.remaining() < 5 {
            return Err(TlvError::ShortRead);
        }

        let t = self.buf[self.pos];
        let s = self.be32_at(self.pos + 1);

        Ok((t, s))
    }

    /// Reads a prolog field `[5][4][tag]` and returns the tag.
    pub fn read_prolog_tag(&mut self) -> Result<u32, TlvError> {
        self.read_header(5, 4)?;

        Ok(self.take_be32())
    }

    /// Reads a u32 field `[2][4][v]`.
    pub fn read_u32(&mut self) -> Result<u32, TlvError> {
        self.read_header(2, 4)?;

        Ok(self.take_be32())
    }

    /// Reads a Q16.16 field `[3][4][v]` as a float.
    pub fn read_q16(&mut self) -> Result<f64, TlvError> {
        self.read_header(3, 4)?;

        Ok(self.take_be32() as i32 as f64 / 65536.0)
    }

    /// Reads a Q42 field `[4][8][v]` as a float.
    pub fn read_q42(&mut self) -> Result<f64, TlvError> {
        self.read_header(4, 8)?;

        Ok(q42_decode(self.take_be64() as i64))
    }

    /// Reads an s64 field `[6][8][v]`.
    pub fn read_s64(&mut self) -> Result<i64, TlvError> {
        self.read_header(6, 8)?;

        Ok(self.take_be64() as i64)
    }

    /// Reads a 2D point: prolog `TAG_POINT2` then two Q42 values.
    pub fn read_point2(&mut self) -> Result<[f64; 2], TlvError> {
        let tag = self.read_prolog_tag()?;
        if tag != TAG_POINT2 {
            return Err(TlvError::WrongTag);
        }

        Ok([self.read_q42()?, self.read_q42()?])
    }

    /// Reads a 3D point: prolog `TAG_POINT3` then three Q42 values.
    pub fn read_point3(&mut self) -> Result<[f64; 3], TlvError> {
        let tag = self.read_prolog_tag()?;
        if tag != TAG_POINT3 {
            return Err(TlvError::WrongTag);
        }

        Ok([self.read_q42()?, self.read_q42()?, self.read_q42()?])
    }

    /// Reads an xds row prolog and returns the column count packed into its tag.
    pub fn read_xds_row(&mut self) -> Result<u32, TlvError> {
        let tag = self.read_prolog_tag()?;
        if tag & 0xffff != 0x0bb8 {
            return Err(TlvError::WrongTag);
        }

        Ok((tag >> 16) & 0xfff)
    }

    /// Reads an xds column prolog plus its u32 column id.
    pub fn read_xds_column(&mut self) -> Result<u32, TlvError> {
        let tag = self.read_prolog_tag()?;
        if tag != 0x020bb9 {
            return Err(TlvError::WrongTag);
        }

        self.read_u32()
    }
}

impl<'a> TlvReader<'a> {
    /// Consumes and validates a `[type][size]` header.
    fn read_header(&mut self, want_type: u8, want_size: u32) -> Result<(), TlvError> {
        let (t, s) = self.peek_header()?;
        if t != want_type {
            return Err(TlvError::WrongType);
        }

        if s != want_size {
            return Err(TlvError::WrongSize);
        }

        // The body must be fully present before the header is consumed, so a failed
        // read never leaves the position inside a field.
        if self.remaining() < 5 + s as usize {
            return Err(TlvError::ShortRead);
        }

        self.pos += 5;

        Ok(())
    }

    /// Big-endian u32 at an absolute position. Caller guarantees bounds.
    fn be32_at(&self, at: usize) -> u32 {
        u32::from_be_bytes([self.buf[at], self.buf[at + 1], self.buf[at + 2], self.buf[at + 3]])
    }

    /// Consumes 4 bytes as big-endian u32. Caller guarantees bounds via `read_header`.
    fn take_be32(&mut self) -> u32 {
        let v = self.be32_at(self.pos);
        self.pos += 4;

        v
    }

    /// Consumes 8 bytes as big-endian u64. Caller guarantees bounds via `read_header`.
    fn take_be64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.buf[self.pos..self.pos + 8]);
        self.pos += 8;

        u64::from_be_bytes(b)
    }
}

// --- Inbound reassembly ---

/// One complete inbound TTP frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub magic   : u32,
    pub seq     : u32,
    pub op      : u32,
    pub payload : Vec<u8>,
}

/// Reassembles inbound USB transfers into complete TTP frames.
///
/// Two layers, because the device speaks in *transfers* and libusb hands back *reads*
/// that need not align with them:
///
/// - The envelope layer. Every device transfer starts with `[0x01, 0, 0, 0]
///   [len: u32 LE]` where `len` is the whole transfer including the envelope. A large
///   response is several transfers (measured on a calibration blob: a header-only
///   transfer, one ~751 KB transfer spanning dozens of reads, a small tail), and a
///   read can start mid-transfer, contain several transfers, or split an envelope.
///   This layer strips envelopes at the exact offsets the declared lengths dictate.
/// - The TTP layer. The de-enveloped byte stream is a sequence of TTP frames; the
///   header's `plen` delimits them.
#[derive(Default)]
pub struct FrameAccumulator {
    /// Partially received envelope header (a read can end inside one).
    env_partial        : [u8; ENVELOPE_SIZE],
    /// Bytes of `env_partial` filled so far.
    env_have           : usize,
    /// Payload bytes left in the current transfer before the next envelope.
    transfer_remaining : usize,
    /// De-enveloped TTP byte stream holding the in-progress frame.
    stream             : Vec<u8>,
}

impl FrameAccumulator {
    /// Creates an empty accumulator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one USB read; complete frames are appended to `out`. A framing error
    /// clears all state (the stream is unrecoverable mid-frame) and is returned; the
    /// stream re-synchronises at the next read that starts a fresh transfer.
    pub fn feed(&mut self, chunk: &[u8], out: &mut Vec<Frame>) -> Result<(), FrameError> {
        let result = self.feed_inner(chunk, out);

        if result.is_err() {
            self.reset();
        }

        result
    }

    /// Discards any partial state (after a reconnect or a framing error).
    pub fn reset(&mut self) {
        self.env_have           = 0;
        self.transfer_remaining = 0;
        self.stream.clear();
    }
}

impl FrameAccumulator {
    /// The feed body; errors propagate up to `feed`, which resets.
    fn feed_inner(&mut self, chunk: &[u8], out: &mut Vec<Frame>)
        -> Result<(), FrameError>
    {
        let mut i = 0usize;

        while i < chunk.len() {
            if self.transfer_remaining == 0 {
                // Between transfers: assemble the next 8-byte envelope, which may be
                // split across reads.
                let take = (ENVELOPE_SIZE - self.env_have).min(chunk.len() - i);
                self.env_partial[self.env_have..self.env_have + take]
                    .copy_from_slice(&chunk[i..i + take]);
                self.env_have += take;
                i += take;

                if self.env_have < ENVELOPE_SIZE {
                    break;
                }

                self.env_have = 0;

                if self.env_partial[0] != 0x01 {
                    return Err(FrameError::BadDirection(self.env_partial[0]));
                }

                let len = u32::from_le_bytes([
                    self.env_partial[4],
                    self.env_partial[5],
                    self.env_partial[6],
                    self.env_partial[7],
                ]) as usize;

                // The declared length covers the envelope itself; anything smaller
                // cannot be a transfer.
                if !(ENVELOPE_SIZE..=ENVELOPE_SIZE + TTP_HDR_SIZE + FRAME_MAX).contains(&len) {
                    return Err(FrameError::BadLength(len as u32));
                }

                self.transfer_remaining = len - ENVELOPE_SIZE;
            }
            else {
                // Inside a transfer: everything up to its declared end is TTP bytes.
                let take = self.transfer_remaining.min(chunk.len() - i);
                self.stream.extend_from_slice(&chunk[i..i + take]);
                self.transfer_remaining -= take;
                i += take;

                self.drain_frames(out)?;
            }
        }

        Ok(())
    }

    /// Emits every complete frame at the head of the TTP stream.
    fn drain_frames(&mut self, out: &mut Vec<Frame>) -> Result<(), FrameError> {
        loop {
            // Validate the magic as soon as it is visible; garbage here means the
            // envelope layer lost sync and waiting for a bogus plen would stall. The
            // device emits magics beyond the documented 0x51..0x53 (0x4e observed at
            // calibration-mode transitions), so only the three always-zero high bytes
            // are checked; unknown magics parse normally and are ignored on dispatch.
            if self.stream.len() >= 4 {
                let magic = be32(&self.stream[0..]);

                if magic > 0xff {
                    return Err(FrameError::BadMagic(magic));
                }
            }

            if self.stream.len() < TTP_HDR_SIZE {
                return Ok(());
            }

            let plen = be32(&self.stream[20..]) as usize;

            if plen > FRAME_MAX {
                return Err(FrameError::BadLength(plen as u32));
            }

            let frame_size = TTP_HDR_SIZE + plen;

            if self.stream.len() < frame_size {
                return Ok(());
            }

            out.push(Frame {
                magic   : be32(&self.stream[0..]),
                seq     : be32(&self.stream[4..]),
                op      : be32(&self.stream[12..]),
                payload : self.stream[TTP_HDR_SIZE..frame_size].to_vec(),
            });

            self.stream.drain(..frame_size);
        }
    }
}

/// Big-endian u32 at the start of a slice. Caller guarantees four bytes.
fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

// --- Errors ---

/// TLV decoding failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TlvError {
    #[error("payload ended inside a field")]
    ShortRead,
    #[error("unexpected field type")]
    WrongType,
    #[error("unexpected field size")]
    WrongSize,
    #[error("unexpected struct tag")]
    WrongTag,
}

/// Inbound framing failure. The accumulator resets itself; the caller decides whether
/// to keep reading or reconnect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("bad direction byte 0x{0:02x} at a transfer boundary")]
    BadDirection(u8),
    #[error("impossible transfer or frame length {0}")]
    BadLength(u32),
    #[error("bad TTP magic 0x{0:08x} (stream desynchronised)")]
    BadMagic(u32),
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a fake inbound transfer: envelope + header + payload.
    fn inbound(magic: u32, seq: u32, op: u32, payload: &[u8]) -> Vec<u8> {
        let total = (ENVELOPE_SIZE + TTP_HDR_SIZE + payload.len()) as u32;
        let mut out = vec![0x01, 0, 0, 0];
        out.extend_from_slice(&total.to_le_bytes());
        out.extend_from_slice(&magic.to_be_bytes());
        out.extend_from_slice(&seq.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&op.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);

        out
    }

    #[test]
    fn point_suggestion_is_an_empty_command() {
        let buf = cal_point_suggestion(7);

        // Envelope(8) + header(24) + the two-byte empty payload.
        assert_eq!(buf.len(), 34);
        assert_eq!(&buf[20..24], &[0, 0, 0x04, 0x42]);
    }

    #[test]
    fn a_point_suggestion_decodes_both_plausible_shapes() {
        // Status prefix, then one prolog'd point.
        let mut prologed = vec![0x00, 0x00];
        put_tag(&mut prologed, TAG_POINT2);
        put_q42(&mut prologed, 0.25);
        put_q42(&mut prologed, 0.75);

        // Q42 is exact for these, being a dyadic fraction of 2^42.
        assert_eq!(decode_point_suggestion(&prologed), vec![[0.25, 0.75]]);

        // Status prefix, then two bare Q42 pairs. These do not round-trip exactly, so
        // the check is on the shape and the values to fixed-point precision.
        let mut bare = vec![0x00, 0x00];
        for v in [0.1, 0.2, 0.3, 0.4] {
            put_q42(&mut bare, v);
        }

        let decoded = decode_point_suggestion(&bare);
        assert_eq!(decoded.len(), 2);

        for (got, want) in decoded.iter().flatten().zip([0.1, 0.2, 0.3, 0.4]) {
            assert!((got - want).abs() < 1e-9, "{got} vs {want}");
        }

        // Anything else yields nothing rather than nonsense.
        assert!(decode_point_suggestion(&[0x00, 0x00]).is_empty());
        assert!(decode_point_suggestion(&[0x00, 0x00, 0xff, 0xff]).is_empty());
        assert!(decode_point_suggestion(&[]).is_empty());
    }

    #[test]
    fn q42_matches_reference() {
        assert_eq!(q42_encode(200.0), 879609302220800);
        assert_eq!(q42_encode(0.0), 0);
        assert_eq!(q42_encode(-200.0), -879609302220800);
        assert!((q42_decode(q42_encode(0.123456)) - 0.123456).abs() < 1e-12);
    }

    #[test]
    fn hello_frame_matches_reference() {
        let buf = hello(1);
        // envelope(8) + header(24) + payload(47).
        assert_eq!(buf.len(), 79);
        assert_eq!(buf[0], 0x00);
        // LE envelope length = 71.
        assert_eq!(&buf[4..8], &[71, 0, 0, 0]);
        // Magic 0x51 big endian.
        assert_eq!(&buf[8..12], &[0, 0, 0, 0x51]);
        // Seq 1.
        assert_eq!(&buf[12..16], &[0, 0, 0, 1]);
        // Op 0x3e8.
        assert_eq!(&buf[20..24], &[0, 0, 0x03, 0xe8]);
        // Payload length 47.
        assert_eq!(&buf[28..32], &[0, 0, 0, 47]);
        assert_eq!(buf[32], 0x00);
    }

    #[test]
    fn subscribe_frame_carries_stream_id() {
        let buf = subscribe(3, 0x500);
        assert_eq!(buf.len(), 52);
        assert_eq!(&buf[22..24], &[0x04, 0xc4]);
        assert_eq!(buf[41], 0x05);
        assert_eq!(buf[42], 0x00);
    }

    #[test]
    fn set_display_area_matches_reference() {
        let rect = DisplayRect { w_mm: 400.0, h_mm: 300.0, ox_mm: -200.0, oy_mm: 0.0, z_mm: 0.0 };
        let buf  = set_display_area(2, rect);
        // envelope(8) + header(24) + payload(2 + 3*48 + 9 + 9) = 196.
        assert_eq!(buf.len(), 196);
        assert_eq!(&buf[22..24], &[0x05, 0xa0]);
    }

    #[test]
    fn display_area_round_trips() {
        let area = DisplayArea {
            tl_mm : [-118.5, 0.0, 0.0],
            tr_mm : [118.5, 0.0, 0.0],
            bl_mm : [-118.5, -148.0, 0.0],
        };
        let buf = set_display_area_corners(7, area);
        // The request payload after the two-byte prefix decodes with the same reader
        // the response path uses.
        let payload = &buf[ENVELOPE_SIZE + TTP_HDR_SIZE..];
        let decoded = decode_display_area(payload).expect("decode");
        assert!((decoded.tl_mm[0] - area.tl_mm[0]).abs() < 1e-9);
        assert!((decoded.bl_mm[1] - area.bl_mm[1]).abs() < 1e-9);
    }

    #[test]
    fn realm_frames_match_reference() {
        assert_eq!(query_realm(5).len(), 34);
        assert_eq!(&query_realm(5)[22..24], &[0x06, 0x40]);

        let open = open_realm(5, 1);
        assert_eq!(open.len(), 44);
        assert_eq!(&open[22..24], &[0x07, 0x6c]);
        assert_eq!(open[34], 0x02);
        assert_eq!(open[42], 0x01);
        assert_eq!(open[43], 0x00);

        let digest: [u8; 16] = core::array::from_fn(|i| i as u8 + 1);
        let resp = realm_response(5, 42, 7, &digest);
        assert_eq!(resp.len(), 68);
        assert_eq!(&resp[22..24], &[0x07, 0x76]);
        assert_eq!(resp[52], 1);
        assert_eq!(resp[67], 16);

        let close = close_realm(5, 42);
        assert_eq!(close.len(), 43);
        assert_eq!(&close[22..24], &[0x07, 0x7b]);
    }

    #[test]
    fn cal_frames_match_reference() {
        let add = cal_point_add2d(5, 0.5, 0.5, 3);
        // envelope(8) + header(24) + payload(2 + 13 + 13 + 9) = 69.
        assert_eq!(add.len(), 69);
        assert_eq!(&add[22..24], &[0x04, 0x06]);

        assert_eq!(cal_retrieve(5).len(), 34);
        assert_eq!(&cal_retrieve(5)[22..24], &[0x04, 0x4c]);

        let apply = cal_apply(5, &[0xaa; 100]);
        assert_eq!(apply.len(), ENVELOPE_SIZE + TTP_HDR_SIZE + 2 + 100);
        assert_eq!(&apply[22..24], &[0x04, 0x56]);
        assert_eq!(apply[ENVELOPE_SIZE + TTP_HDR_SIZE + 2], 0xaa);
    }

    #[test]
    fn bare_u32_setting_has_no_prefix() {
        let buf = set_u32(9, OP_ENABLED_EYE_SET, 3);
        assert_eq!(buf.len(), ENVELOPE_SIZE + TTP_HDR_SIZE + 9);
        // Payload starts directly with the TLV type byte, no [00 00] prefix.
        assert_eq!(buf[ENVELOPE_SIZE + TTP_HDR_SIZE], 2);
        assert_eq!(buf[ENVELOPE_SIZE + TTP_HDR_SIZE + 8], 3);
    }

    #[test]
    fn accumulator_parses_single_frame() {
        let mut acc = FrameAccumulator::new();
        let mut out = Vec::new();
        acc.feed(&inbound(MAGIC_RSP, 42, 0x3e8, &[0xde, 0xad, 0xbe, 0xef]), &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].magic, MAGIC_RSP);
        assert_eq!(out[0].seq, 42);
        assert_eq!(out[0].op, 0x3e8);
        assert_eq!(out[0].payload, vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn accumulator_parses_concatenated_frames() {
        let mut chunk = inbound(MAGIC_RSP, 1, 0x100, &[0x11]);
        chunk.extend_from_slice(&inbound(MAGIC_NOTIFY, 0, 0x500, &[0x22, 0x23]));

        let mut acc = FrameAccumulator::new();
        let mut out = Vec::new();
        acc.feed(&chunk, &mut out).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].op, 0x500);
        assert_eq!(out[1].payload[0], 0x22);
    }

    #[test]
    fn accumulator_parses_split_frame() {
        let full = inbound(MAGIC_RSP, 7, 0x200, &[0xa1, 0xa2, 0xa3, 0xa4]);

        let mut acc = FrameAccumulator::new();
        let mut out = Vec::new();
        acc.feed(&full[..20], &mut out).unwrap();
        assert!(out.is_empty());
        acc.feed(&full[20..], &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].seq, 7);
    }

    #[test]
    fn accumulator_rejects_bad_direction() {
        let mut acc = FrameAccumulator::new();
        let mut out = Vec::new();
        let err = acc.feed(&[0x02, 0, 0, 0, 0x20, 0, 0, 0], &mut out).unwrap_err();
        assert_eq!(err, FrameError::BadDirection(0x02));
        // The accumulator reset, so a good frame parses afterwards.
        acc.feed(&inbound(MAGIC_RSP, 1, 0x100, &[]), &mut out).unwrap();
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn accumulator_rejects_impossible_transfer_length() {
        let mut acc = FrameAccumulator::new();
        let mut out = Vec::new();
        // A transfer cannot be shorter than its own envelope.
        let err = acc.feed(&[0x01, 0, 0, 0, 7, 0, 0, 0], &mut out).unwrap_err();
        assert_eq!(err, FrameError::BadLength(7));
    }

    #[test]
    fn accumulator_rejects_garbage_magic() {
        // A valid envelope whose payload is not a TTP frame: the magic check catches
        // the desync instead of trusting a bogus plen.
        let mut chunk = vec![0x01, 0, 0, 0];
        chunk.extend_from_slice(&40u32.to_le_bytes());
        chunk.extend_from_slice(&[0xde; 32]);

        let mut acc = FrameAccumulator::new();
        let mut out = Vec::new();
        let err = acc.feed(&chunk, &mut out).unwrap_err();
        assert_eq!(err, FrameError::BadMagic(0xdededede));
    }

    #[test]
    fn accumulator_handles_split_envelope() {
        let full = inbound(MAGIC_RSP, 5, 0x300, &[0x42; 10]);

        let mut acc = FrameAccumulator::new();
        let mut out = Vec::new();
        // The 8-byte envelope itself arrives split across reads (observed on the
        // device: an 8-byte read followed by the transfer body).
        acc.feed(&full[..5], &mut out).unwrap();
        acc.feed(&full[5..8], &mut out).unwrap();
        assert!(out.is_empty());
        acc.feed(&full[8..], &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].payload, vec![0x42; 10]);
    }

    #[test]
    fn accumulator_reassembles_multi_transfer_frame() {
        // The measured calibration-blob shape: a header-only transfer carrying the
        // TTP header plus the first few payload bytes, one large transfer whose
        // envelope appears only at its start while its body spans several reads, and
        // a small tail transfer. Envelope lengths include the envelope itself.
        let payload: Vec<u8> = (0..200u32).map(|i| (i as u8) | 0x20).collect();

        // Transfer 1: envelope + TTP header + payload[..11].
        let mut t1 = vec![0x01, 0, 0, 0];
        t1.extend_from_slice(&(43u32).to_le_bytes());
        t1.extend_from_slice(&MAGIC_RSP.to_be_bytes());
        t1.extend_from_slice(&99u32.to_be_bytes());
        t1.extend_from_slice(&0u32.to_be_bytes());
        t1.extend_from_slice(&0x44cu32.to_be_bytes());
        t1.extend_from_slice(&0u32.to_be_bytes());
        t1.extend_from_slice(&200u32.to_be_bytes());
        t1.extend_from_slice(&payload[..11]);

        // Transfer 2: envelope + payload[11..161] (150 bytes).
        let mut t2 = vec![0x01, 0, 0, 0];
        t2.extend_from_slice(&(8u32 + 150).to_le_bytes());
        t2.extend_from_slice(&payload[11..161]);

        // Transfer 3: envelope + payload[161..] (39 bytes).
        let mut t3 = vec![0x01, 0, 0, 0];
        t3.extend_from_slice(&(8u32 + 39).to_le_bytes());
        t3.extend_from_slice(&payload[161..]);

        // Reads misaligned with transfers: t1 whole, then t2's envelope alone, then
        // t2's body in two pieces, the second piece running into t3.
        let mut acc = FrameAccumulator::new();
        let mut out = Vec::new();
        acc.feed(&t1, &mut out).unwrap();
        assert!(out.is_empty());
        acc.feed(&t2[..8], &mut out).unwrap();
        acc.feed(&t2[8..108], &mut out).unwrap();
        let mut tail = t2[108..].to_vec();
        tail.extend_from_slice(&t3);
        acc.feed(&tail, &mut out).unwrap();

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].seq, 99);
        assert_eq!(out[0].op, 0x44c);
        assert_eq!(out[0].payload, payload);
    }
}
