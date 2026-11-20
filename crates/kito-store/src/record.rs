//! Format of the records in log files: `length + payload + CRC32C`.
//!
//! ```text
//! file    = header record*
//! header  = "KITO" kind:u8 version:u8 0:u16 segment:u32le crc32c(byte 0..12):u32le
//! record  = len:u32le payload[len] crc32c(len ++ payload):u32le
//! ```
//!
//! The CRC also covers the length: a damaged length cannot cause a wrong
//! record to be read as valid. A tail of zeros is not a valid record,
//! because the CRC32C of four zero bytes is not zero.

pub(crate) const LEN_SIZE: usize = 4;
pub(crate) const CRC_SIZE: usize = 4;
/// Bytes added to every payload.
pub const RECORD_OVERHEAD: usize = LEN_SIZE + CRC_SIZE;
/// Maximum payload of a record (2 GiB − 1).
pub const MAX_PAYLOAD: usize = i32::MAX as usize;

pub(crate) const HEADER_LEN: usize = 16;
const MAGIC: &[u8; 4] = b"KITO";
const FORMAT_VERSION: u8 = 1;

/// Kind of log, written in the header of every file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LogKind {
    Oplog = 1,
    Pack = 2,
    Checkpoints = 3,
    Intents = 4,
    Lanes = 5,
    Conflicts = 6,
    /// Notes on code (ADR 0003).
    Notes = 7,
}

pub(crate) fn encode_header(kind: LogKind, segment: u32) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[..4].copy_from_slice(MAGIC);
    h[4] = kind as u8;
    h[5] = FORMAT_VERSION;
    h[8..12].copy_from_slice(&segment.to_le_bytes());
    let crc = crc32c::crc32c(&h[..12]);
    h[12..].copy_from_slice(&crc.to_le_bytes());
    h
}

pub(crate) fn check_header(data: &[u8], kind: LogKind, segment: u32) -> Result<(), String> {
    let Some(h) = data.get(..HEADER_LEN) else {
        return Err(format!("incomplete header ({} bytes)", data.len()));
    };
    if &h[..4] != MAGIC {
        return Err("wrong magic".into());
    }
    let crc = u32::from_le_bytes(h[12..16].try_into().unwrap());
    if crc != crc32c::crc32c(&h[..12]) {
        return Err("wrong header CRC".into());
    }
    if h[5] != FORMAT_VERSION {
        return Err(format!("unsupported format version {}", h[5]));
    }
    if h[4] != kind as u8 {
        return Err(format!("unexpected log kind {} (expected {})", h[4], kind as u8));
    }
    let found = u32::from_le_bytes(h[8..12].try_into().unwrap());
    if found != segment {
        return Err(format!("unexpected segment number {found} (expected {segment})"));
    }
    Ok(())
}

fn crc_of(len: [u8; LEN_SIZE], parts: &[&[u8]]) -> u32 {
    parts.iter().fold(crc32c::crc32c(&len), |crc, p| crc32c::crc32c_append(crc, p))
}

/// On-disk size of a record with this payload.
pub(crate) fn framed_len(payload_len: usize) -> u64 {
    (payload_len + RECORD_OVERHEAD) as u64
}

/// Appends to `out` a record whose payload is the concatenation of `parts`.
/// Returns `Err(length)` if the payload exceeds [`MAX_PAYLOAD`].
pub(crate) fn encode_record(out: &mut Vec<u8>, parts: &[&[u8]]) -> Result<(), usize> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    if len > MAX_PAYLOAD {
        return Err(len);
    }
    let len_le = (len as u32).to_le_bytes();
    out.reserve(len + RECORD_OVERHEAD);
    out.extend_from_slice(&len_le);
    for p in parts {
        out.extend_from_slice(p);
    }
    out.extend_from_slice(&crc_of(len_le, parts).to_le_bytes());
    Ok(())
}

/// Outcome of reading a record at a given offset.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Frame<'a> {
    /// Valid record; `end` is the offset of the next record.
    Record { payload: &'a [u8], crc: u32, end: usize },
    /// Clean end: the offset coincides with the end of the data.
    End,
    /// From here on the data is not a valid record (incomplete or wrong CRC).
    Torn(&'static str),
}

pub(crate) fn read_frame(data: &[u8], offset: usize) -> Frame<'_> {
    let Some(rest) = data.get(offset..) else {
        return Frame::Torn("offset past the end of the data");
    };
    if rest.is_empty() {
        return Frame::End;
    }
    let Some(len_le) = rest.first_chunk::<LEN_SIZE>() else {
        return Frame::Torn("incomplete length");
    };
    let len = u32::from_le_bytes(*len_le) as usize;
    if len > MAX_PAYLOAD {
        return Frame::Torn("implausible length");
    }
    let Some(body) = rest.get(LEN_SIZE..LEN_SIZE + len + CRC_SIZE) else {
        return Frame::Torn("incomplete record");
    };
    let (payload, crc_le) = body.split_at(len);
    let crc = u32::from_le_bytes(crc_le.try_into().unwrap());
    if crc != crc_of(*len_le, &[payload]) {
        return Frame::Torn("wrong CRC");
    }
    Frame::Record { payload, crc, end: offset + LEN_SIZE + len + CRC_SIZE }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn header_round_trip_and_rejections() {
        let h = encode_header(LogKind::Oplog, 7);
        assert!(check_header(&h, LogKind::Oplog, 7).is_ok());
        assert!(check_header(&h, LogKind::Pack, 7).is_err());
        assert!(check_header(&h, LogKind::Oplog, 8).is_err());
        assert!(check_header(&h[..10], LogKind::Oplog, 7).is_err());
        for i in 0..HEADER_LEN {
            let mut bad = h;
            bad[i] ^= 0x40;
            assert!(check_header(&bad, LogKind::Oplog, 7).is_err(), "byte {i}");
        }
    }

    #[test]
    fn zero_filled_tail_is_not_a_record() {
        assert!(matches!(read_frame(&[0u8; 64], 0), Frame::Torn(_)));
        assert!(matches!(read_frame(&[0u8; 8], 0), Frame::Torn(_)));
    }

    #[test]
    fn empty_payload_is_a_valid_record() {
        let mut buf = Vec::new();
        encode_record(&mut buf, &[]).unwrap();
        assert_eq!(buf.len(), RECORD_OVERHEAD);
        assert!(matches!(read_frame(&buf, 0), Frame::Record { payload: &[], end: 8, .. }));
        assert_eq!(read_frame(&buf, 8), Frame::End);
    }

    proptest! {
        #[test]
        fn records_round_trip(payloads in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..300), 0..20)) {
            let mut buf = Vec::new();
            for p in &payloads {
                let (a, b) = p.split_at(p.len() / 2);
                encode_record(&mut buf, &[a, b]).unwrap();
            }
            let mut off = 0;
            for p in &payloads {
                match read_frame(&buf, off) {
                    Frame::Record { payload, end, .. } => {
                        prop_assert_eq!(payload, &p[..]);
                        prop_assert_eq!(end as u64, off as u64 + framed_len(p.len()));
                        off = end;
                    }
                    other => prop_assert!(false, "expected a record, found {:?}", other),
                }
            }
            prop_assert_eq!(read_frame(&buf, off), Frame::End);
        }

        /// Truncating at any point leaves readable all and only the records
        /// that were written in full.
        #[test]
        fn truncation_keeps_exactly_the_complete_records(
            payloads in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..64), 1..12),
            cut in any::<prop::sample::Index>(),
        ) {
            let mut buf = Vec::new();
            let mut ends = Vec::new();
            for p in &payloads {
                encode_record(&mut buf, &[p]).unwrap();
                ends.push(buf.len());
            }
            let cut = cut.index(buf.len() + 1);
            let data = &buf[..cut];
            let complete = ends.iter().filter(|&&e| e <= cut).count();
            let mut off = 0;
            let mut seen = 0;
            loop {
                match read_frame(data, off) {
                    Frame::Record { payload, end, .. } => {
                        prop_assert_eq!(payload, &payloads[seen][..]);
                        seen += 1;
                        off = end;
                    }
                    Frame::End => { prop_assert_eq!(off, cut); break; }
                    Frame::Torn(_) => break,
                }
            }
            prop_assert_eq!(seen, complete);
        }

        /// A single flipped bit never goes unnoticed.
        #[test]
        fn any_bit_flip_is_detected(
            payload in prop::collection::vec(any::<u8>(), 0..200),
            byte in any::<prop::sample::Index>(),
            bit in 0u8..8,
        ) {
            let mut buf = Vec::new();
            encode_record(&mut buf, &[&payload]).unwrap();
            let i = byte.index(buf.len());
            buf[i] ^= 1 << bit;
            match read_frame(&buf, 0) {
                Frame::Record { payload: got, end, .. } => {
                    // The only way to stay "valid" would be a CRC collision.
                    prop_assert!(false, "bit flip not detected: {} bytes, end {}", got.len(), end);
                }
                Frame::End => prop_assert!(false, "unexpected End"),
                Frame::Torn(_) => {}
            }
        }
    }
}
