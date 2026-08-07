//! PMK chunk framing — magic, tags, sequential writer, chunk iterator.
//! Spec: `../docs/pmk-spec.md` §2, §3, §13.

use anyhow::{Context, Result};

pub const MAGIC: [u8; 4] = *b"PMK1";

pub const TAG_META: [u8; 4] = *b"META";
pub const TAG_INFO: [u8; 4] = *b"INFO";
pub const TAG_BPM: [u8; 4] = *b"BPM ";
pub const TAG_LINE: [u8; 4] = *b"LINE";
pub const TAG_NOTE: [u8; 4] = *b"NOTE";
pub const TAG_EVNT: [u8; 4] = *b"EVNT";
pub const TAG_XEVT: [u8; 4] = *b"XEVT";
pub const TAG_CTRL: [u8; 4] = *b"CTRL";
pub const TAG_END: [u8; 4] = *b"END ";

/// Tags tracked by the END index (random-access targets).
pub const INDEXED_TAGS: [[u8; 4]; 4] = [TAG_NOTE, TAG_EVNT, TAG_XEVT, TAG_CTRL];

#[derive(Clone, Debug, PartialEq)]
pub struct Chunk {
    pub tag: [u8; 4],
    pub payload: Vec<u8>,
}

pub fn tag_str(tag: [u8; 4]) -> String {
    String::from_utf8_lossy(&tag).trim_end().to_string()
}

/// Sequential chunk iterator over a full PMK buffer (magic already stripped).
pub struct ChunkIter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ChunkIter<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    /// Read the next chunk, or `None` at end of buffer.
    pub fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        if self.pos >= self.data.len() {
            return Ok(None);
        }
        let offset = self.pos;
        let header = self.data.get(self.pos..self.pos + 8).context(format!(
            "chunk header truncated at 0x{offset:x}"
        ))?;
        let mut tag = [0u8; 4];
        tag.copy_from_slice(&header[..4]);
        self.pos += 8;
        let len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let payload = self.data.get(self.pos..self.pos + len).context(format!(
            "chunk {:?} at 0x{offset:x}: length {len} overruns file",
            tag_str(tag)
        ))?;
        self.pos += len;
        Ok(Some(Chunk { tag, payload: payload.to_vec() }))
    }
}

/// Sequential writer that records chunk header offsets (for the END index).
#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
    pub index: Vec<(u32, u64)>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one chunk; returns its header offset and records it in the index.
    pub fn chunk(&mut self, tag: [u8; 4], payload: &[u8]) -> u64 {
        let offset = self.buf.len() as u64;
        self.buf.extend_from_slice(&tag);
        self.buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(payload);
        self.index.push((u32::from_le_bytes(tag), offset));
        offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_roundtrip() {
        let mut w = Writer::new();
        w.chunk(TAG_META, b"hello");
        w.chunk(*b"TEST", b"\x00\x01\x02");
        let mut it = ChunkIter::new(&w.buf);
        let c1 = it.next_chunk().unwrap().unwrap();
        assert_eq!(c1.tag, TAG_META);
        assert_eq!(c1.payload, b"hello");
        let c2 = it.next_chunk().unwrap().unwrap();
        assert_eq!(c2.tag, *b"TEST");
        assert_eq!(c2.payload, vec![0u8, 1, 2]);
        assert!(it.next_chunk().unwrap().is_none());
    }

    #[test]
    fn truncated_header_reported() {
        let mut it = ChunkIter::new(b"MET");
        assert!(it.next_chunk().unwrap_err().to_string().contains("truncated"));
    }

    #[test]
    fn overrun_reported() {
        let mut w = Writer::new();
        w.chunk(TAG_NOTE, &[0u8; 4]);
        // Corrupt the length to claim more payload than exists.
        w.buf[4..8].copy_from_slice(&1000u32.to_le_bytes());
        let mut it = ChunkIter::new(&w.buf);
        let err = it.next_chunk().unwrap_err().to_string();
        assert!(err.contains("overruns"), "{err}");
    }
}
