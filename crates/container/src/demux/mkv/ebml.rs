/// Raw EBML scanner for matroska-demuxer 0.7 bug workarounds.
///
/// Exposes `scan_mkv_colour_raw` (reads MaxCLL, MaxFALL, and the three
/// buggy y-chromaticity fields straight from the byte stream) and the two
/// `pub(super)` VInt readers that `demux/tests.rs` exercises directly.

// ---------------------------------------------------------------------------
// Workaround result type
// ---------------------------------------------------------------------------

/// Fields recovered by raw EBML scanning to work around two matroska-demuxer
/// 0.7 bugs:
///   * `Colour::new` reads MaxCLL / MaxFALL from the MatrixCoefficients
///     ElementId offset (lib.rs:725..728 in matroska-demuxer-0.7.0/src/lib.rs).
///   * `MasteringMetadata::new` reads `primary_{r,g,b}_chromaticity_y` from
///     the matching X ElementId — all three y values come back holding the
///     corresponding x value.
#[derive(Default)]
pub(super) struct RawColourFix {
    pub(super) max_cll: Option<u32>,
    pub(super) max_fall: Option<u32>,
    /// Mastering display y-chromaticity recoveries — Squad-21.
    pub(super) primary_r_chromaticity_y: Option<f64>,
    pub(super) primary_g_chromaticity_y: Option<f64>,
    pub(super) primary_b_chromaticity_y: Option<f64>,
}

// ---------------------------------------------------------------------------
// Raw EBML colour scan
// ---------------------------------------------------------------------------

/// Raw-bytes EBML walk for the Colour element's MaxCLL (0x55BC),
/// MaxFALL (0x55BD), and the mastering display chromaticity_y fields
/// (0x55D2 / 0x55D4 / 0x55D6). Used exclusively as a workaround for
/// matroska-demuxer 0.7 bugs (see `RawColourFix`).
/// Returns `None` when the file is not well-formed enough to reach the
/// Colour element, or when neither bug-recovery field is present.
pub(super) fn scan_mkv_colour_raw(data: &[u8]) -> Option<RawColourFix> {
    // Top-level: EBML header (0x1A45DFA3) then Segment (0x18538067).
    // We walk linearly until we find the Segment element and grab its
    // payload bytes — all subsequent work is inside that slice.
    let mut cursor = 0;
    let seg_body: &[u8] = loop {
        let (el, after) = next_ebml_element(data, cursor)?;
        if el.id == 0x18538067 {
            break &data[el.body_start..el.body_start + el.body_len];
        }
        cursor = after;
    };

    // Segment → Tracks (0x1654AE6B). Segment may carry many top-level
    // elements in any order — walk them until we find Tracks.
    let tracks = find_ebml_child(seg_body, 0x1654AE6B)?;
    // Tracks → TrackEntry* (0xAE). Look for the first TrackEntry whose
    // Video sub-element has a Colour; that's the path we care about.
    let mut cur = 0;
    while cur < tracks.len() {
        let (el, after) = next_ebml_element(tracks, cur)?;
        cur = after;
        if el.id != 0xAE {
            continue;
        }
        let entry = &tracks[el.body_start..el.body_start + el.body_len];
        let Some(video) = find_ebml_child(entry, 0xE0) else {
            continue;
        };
        let Some(colour) = find_ebml_child(video, 0x55B0) else {
            continue;
        };

        let mut fix = RawColourFix::default();
        let mut c = 0;
        while c < colour.len() {
            let (ce, after_ce) = match next_ebml_element(colour, c) {
                Some(v) => v,
                None => break,
            };
            c = after_ce;
            let value_bytes = &colour[ce.body_start..ce.body_start + ce.body_len];
            match ce.id {
                0x55BC => {
                    fix.max_cll = read_unsigned(value_bytes).and_then(|v| u32::try_from(v).ok());
                }
                0x55BD => {
                    fix.max_fall = read_unsigned(value_bytes).and_then(|v| u32::try_from(v).ok());
                }
                // MasteringMetadata sub-element (0x55D0). Walk its children
                // and pull the three buggy y-chromaticities so callers can
                // override the typed-accessor reads.
                0x55D0 => {
                    let md = value_bytes;
                    let mut mc = 0;
                    while mc < md.len() {
                        let (mce, after_mce) = match next_ebml_element(md, mc) {
                            Some(v) => v,
                            None => break,
                        };
                        mc = after_mce;
                        let mv = &md[mce.body_start..mce.body_start + mce.body_len];
                        match mce.id {
                            0x55D2 => fix.primary_r_chromaticity_y = read_float(mv),
                            0x55D4 => fix.primary_g_chromaticity_y = read_float(mv),
                            0x55D6 => fix.primary_b_chromaticity_y = read_float(mv),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        if fix.max_cll.is_some()
            || fix.max_fall.is_some()
            || fix.primary_r_chromaticity_y.is_some()
            || fix.primary_g_chromaticity_y.is_some()
            || fix.primary_b_chromaticity_y.is_some()
        {
            return Some(fix);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// EBML walking primitives
// ---------------------------------------------------------------------------

/// Walk the direct children of `buf` (assumed to be an EBML master
/// element body, NOT starting with the master's own header) and
/// return the payload slice of the first element with id `want`.
fn find_ebml_child(buf: &[u8], want: u32) -> Option<&[u8]> {
    let mut cur = 0;
    while cur < buf.len() {
        let (el, after) = next_ebml_element(buf, cur)?;
        cur = after;
        if el.id == want {
            return Some(&buf[el.body_start..el.body_start + el.body_len]);
        }
    }
    None
}

#[derive(Debug)]
struct RawEbmlElement {
    id: u32,
    body_start: usize,
    body_len: usize,
}

/// Read a single EBML element at `off` within `buf`. Returns the
/// element descriptor plus the byte offset immediately after the
/// element (header + body). Only handles up to 4-byte IDs (all
/// Matroska elements fit) and size VInts up to 8 bytes.
fn next_ebml_element(buf: &[u8], off: usize) -> Option<(RawEbmlElement, usize)> {
    if off >= buf.len() {
        return None;
    }
    let (id, id_len) = read_id_vint(&buf[off..])?;
    let body_off = off + id_len;
    if body_off >= buf.len() {
        return None;
    }
    let (size, size_len) = read_size_vint(&buf[body_off..])?;
    let body_start = body_off + size_len;
    if body_start + size as usize > buf.len() {
        return None;
    }
    let elem = RawEbmlElement {
        id,
        body_start,
        body_len: size as usize,
    };
    Some((elem, body_start + size as usize))
}

// ---------------------------------------------------------------------------
// VInt readers — pub(super) so demux/tests.rs can reach them via
// `super::mkv::{read_id_vint, read_size_vint}` through mod.rs's re-export.
// ---------------------------------------------------------------------------

/// Read an EBML Class A/B/C/D ID (top-bit marker determines width,
/// 1..=4 bytes). Returns (raw id with marker bits preserved, byte-count).
pub(crate) fn read_id_vint(buf: &[u8]) -> Option<(u32, usize)> {
    if buf.is_empty() {
        return None;
    }
    let first = buf[0];
    let len = if first & 0x80 != 0 {
        1
    } else if first & 0x40 != 0 {
        2
    } else if first & 0x20 != 0 {
        3
    } else if first & 0x10 != 0 {
        4
    } else {
        return None;
    };
    if buf.len() < len {
        return None;
    }
    let mut id: u32 = 0;
    for b in &buf[..len] {
        id = (id << 8) | (*b as u32);
    }
    Some((id, len))
}

/// Read an EBML size VInt (1..=8 bytes). Strips the marker bit and
/// returns the numeric value plus byte-count.
pub(crate) fn read_size_vint(buf: &[u8]) -> Option<(u64, usize)> {
    if buf.is_empty() {
        return None;
    }
    let first = buf[0];
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || buf.len() < len {
        return None;
    }
    // Mask off the leading marker bit. `len == 8` (first byte 0x01) has
    // *no* value bits in the first byte — all 56 value bits live in
    // bytes 1..8. `u8 >> 8` is UB, so branch explicitly.
    let mask: u8 = if len == 8 { 0 } else { 0xFFu8 >> len };
    let mut v: u64 = (first & mask) as u64;
    for b in &buf[1..len] {
        v = (v << 8) | (*b as u64);
    }
    Some((v, len))
}

// ---------------------------------------------------------------------------
// Primitive value readers (private — used only within this file)
// ---------------------------------------------------------------------------

/// Read a big-endian unsigned integer (1..=8 bytes) from a Matroska
/// value payload. Zero-length payloads encode 0.
fn read_unsigned(buf: &[u8]) -> Option<u64> {
    if buf.len() > 8 {
        return None;
    }
    let mut v: u64 = 0;
    for b in buf {
        v = (v << 8) | (*b as u64);
    }
    Some(v)
}

/// Read a big-endian Matroska float payload — 4 bytes encode an f32,
/// 8 bytes encode an f64. Anything else is malformed.
fn read_float(buf: &[u8]) -> Option<f64> {
    match buf.len() {
        4 => {
            let arr: [u8; 4] = buf.try_into().ok()?;
            Some(f32::from_be_bytes(arr) as f64)
        }
        8 => {
            let arr: [u8; 8] = buf.try_into().ok()?;
            Some(f64::from_be_bytes(arr))
        }
        _ => None,
    }
}
