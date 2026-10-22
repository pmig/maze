//! Length-delimited framing with a size limit that keeps the connection usable.

use std::io;

use bytes::{Buf as _, Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder, LengthDelimitedCodec};

/// Bytes in the length prefix.
const HEADER_LEN: usize = 4;

/// One decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// A complete frame's payload.
    Data(BytesMut),
    /// A frame longer than the limit. Its payload was read and thrown away, so the
    /// stream is positioned at the next frame and the connection stays usable.
    Oversized {
        /// The announced payload length.
        len: usize,
    },
}

/// The local API's frame codec: a 4-byte big-endian length prefix followed by the
/// payload, exactly the default [`LengthDelimitedCodec`] layout.
///
/// Unlike a plain `LengthDelimitedCodec`, an oversized frame doesn't poison the
/// stream: the decoder skips its payload without buffering it and yields
/// [`Frame::Oversized`], so the server can answer `frame_too_large` and keep serving
/// the connection. Encoding refuses payloads over the limit.
#[derive(Debug)]
pub struct FrameCodec {
    inner: LengthDelimitedCodec,
    max_frame_len: usize,
    state: DecodeState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecodeState {
    /// At a frame boundary.
    Head,
    /// The inner codec has consumed a header and waits for the payload.
    Payload,
    /// Skipping an oversized payload.
    Discard { len: usize, remaining: usize },
}

impl FrameCodec {
    /// A codec that accepts frames up to `max_frame_len` bytes.
    pub fn new(max_frame_len: usize) -> Self {
        Self {
            inner: LengthDelimitedCodec::builder()
                .length_field_length(HEADER_LEN)
                .max_frame_length(max_frame_len)
                .new_codec(),
            max_frame_len,
            state: DecodeState::Head,
        }
    }

    /// The configured limit.
    pub fn max_frame_len(&self) -> usize {
        self.max_frame_len
    }
}

impl Decoder for FrameCodec {
    type Item = Frame;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Frame>, io::Error> {
        loop {
            match self.state {
                DecodeState::Discard { len, remaining } => {
                    let skip = remaining.min(src.len());
                    src.advance(skip);
                    let remaining = remaining - skip;
                    if remaining == 0 {
                        self.state = DecodeState::Head;
                        return Ok(Some(Frame::Oversized { len }));
                    }
                    self.state = DecodeState::Discard { len, remaining };
                    return Ok(None);
                }
                DecodeState::Head => {
                    let Some(header) = src.get(..HEADER_LEN) else {
                        return Ok(None);
                    };
                    let mut prefix = [0u8; HEADER_LEN];
                    prefix.copy_from_slice(header);
                    let len = usize::try_from(u32::from_be_bytes(prefix)).unwrap_or(usize::MAX);
                    if len > self.max_frame_len {
                        src.advance(HEADER_LEN);
                        self.state = DecodeState::Discard {
                            len,
                            remaining: len,
                        };
                        continue;
                    }
                    self.state = DecodeState::Payload;
                }
                DecodeState::Payload => {
                    return match self.inner.decode(src)? {
                        Some(payload) => {
                            self.state = DecodeState::Head;
                            Ok(Some(Frame::Data(payload)))
                        }
                        None => Ok(None),
                    };
                }
            }
        }
    }
}

impl Encoder<Bytes> for FrameCodec {
    type Error = io::Error;

    fn encode(&mut self, item: Bytes, dst: &mut BytesMut) -> Result<(), io::Error> {
        self.inner.encode(item, dst)
    }
}

#[cfg(test)]
mod tests {
    use bytes::BufMut as _;

    use super::*;

    fn frame(payload: &[u8]) -> BytesMut {
        let mut buf = BytesMut::new();
        buf.put_u32(u32::try_from(payload.len()).unwrap());
        buf.put_slice(payload);
        buf
    }

    #[test]
    fn decodes_split_frames() {
        let mut codec = FrameCodec::new(64);
        let mut wire = frame(b"hello");
        wire.extend_from_slice(&frame(b"world"));
        let mut src = BytesMut::new();
        let mut out = Vec::new();
        // Feed one byte at a time.
        for byte in wire {
            src.put_u8(byte);
            while let Some(frame) = codec.decode(&mut src).unwrap() {
                out.push(frame);
            }
        }
        assert_eq!(
            out,
            vec![
                Frame::Data(BytesMut::from(&b"hello"[..])),
                Frame::Data(BytesMut::from(&b"world"[..]))
            ]
        );
    }

    #[test]
    fn oversized_frame_is_skipped_and_stream_continues() {
        let mut codec = FrameCodec::new(8);
        let mut src = frame(&[b'x'; 100]);
        src.extend_from_slice(&frame(b"ok"));
        // The oversized payload arrives in pieces; it is never buffered.
        let mut partial = src.split_to(50);
        assert_eq!(codec.decode(&mut partial).unwrap(), None);
        assert!(partial.is_empty());
        assert_eq!(
            codec.decode(&mut src).unwrap(),
            Some(Frame::Oversized { len: 100 })
        );
        assert_eq!(
            codec.decode(&mut src).unwrap(),
            Some(Frame::Data(BytesMut::from(&b"ok"[..])))
        );
    }

    #[test]
    fn encodes_with_length_prefix_and_limit() {
        let mut codec = FrameCodec::new(4);
        let mut dst = BytesMut::new();
        codec.encode(Bytes::from_static(b"abc"), &mut dst).unwrap();
        assert_eq!(&dst[..], &[0, 0, 0, 3, b'a', b'b', b'c']);
        assert!(
            codec
                .encode(Bytes::from_static(b"abcde"), &mut dst)
                .is_err()
        );
        assert_eq!(codec.max_frame_len(), 4);
    }
}
