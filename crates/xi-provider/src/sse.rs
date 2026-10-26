use bytes::{Buf, Bytes};
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::time::Duration;

use crate::transport::{decode, network};
use crate::{Error, ErrorKind};

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Event {
    pub(crate) name: String,
    pub(crate) data: String,
}

#[derive(Default)]
struct Decoder {
    line: Vec<u8>,
    name: String,
    data: String,
    has_data: bool,
    skip_lf: bool,
    started: bool,
}

impl Decoder {
    fn next(&mut self, bytes: &mut Bytes) -> Result<Option<Event>, Error> {
        while bytes.has_remaining() {
            let byte = bytes.get_u8();
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte != b'\r' && byte != b'\n' {
                self.line.push(byte);
                continue;
            }
            self.skip_lf = byte == b'\r';
            if let Some(event) = self.finish_line()? {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }

    fn finish_line(&mut self) -> Result<Option<Event>, Error> {
        let line =
            std::str::from_utf8(&self.line).map_err(|_| decode("SSE contains invalid UTF-8"))?;
        let line = if self.started {
            line
        } else {
            line.trim_start_matches('\u{feff}')
        };
        self.started = true;
        if line.is_empty() {
            self.line.clear();
            let name = std::mem::take(&mut self.name);
            if self.has_data {
                self.has_data = false;
                self.data.pop(); // Remove only the newline appended by the last data field.
                return Ok(Some(Event {
                    name,
                    data: std::mem::take(&mut self.data),
                }));
            }
            return Ok(None);
        }
        if !line.starts_with(':') {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => {
                    self.name.clear();
                    self.name.push_str(value);
                }
                "data" => {
                    self.has_data = true;
                    self.data.push_str(value);
                    self.data.push('\n');
                }
                _ => {}
            }
        }
        self.line.clear();
        Ok(None)
    }
}

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

pub(crate) struct Events {
    stream: ByteStream,
    decoder: Decoder,
    bytes: Bytes,
    idle: Duration,
}

impl Events {
    pub(crate) fn new(response: reqwest::Response) -> Self {
        Self {
            stream: Box::pin(response.bytes_stream()),
            decoder: Decoder::default(),
            bytes: Bytes::new(),
            idle: Duration::from_secs(120),
        }
    }

    pub(crate) async fn next(&mut self) -> Result<Option<Event>, Error> {
        loop {
            if let Some(event) = self.decoder.next(&mut self.bytes)? {
                return Ok(Some(event));
            }
            self.bytes = tokio::time::timeout(self.idle, async {
                // Empty chunks do not reset the idle timer.
                loop {
                    match self.stream.next().await {
                        Some(Ok(bytes)) if bytes.is_empty() => continue,
                        result => break result,
                    }
                }
            })
            .await
            .map_err(|_| Error::new(ErrorKind::Network, "Provider stream idle timeout"))?
            .transpose()
            .map_err(network)?
            .unwrap_or_default();
            if self.bytes.is_empty() {
                // SSE does not dispatch an event without its terminating blank line.
                return Ok(None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_chunk_boundary_including_utf8_crlf_and_multiline() {
        let source = "\u{feff}: comment\r\nevent: custom\r\ndata: {\"text\":\r\ndata: \"héllo\"}\r\n\r\n: ping\ndata\n\nevent: discarded\n\ndata: last\r\r";
        let expected = vec![
            Event {
                name: "custom".into(),
                data: "{\"text\":\n\"héllo\"}".into(),
            },
            Event {
                name: String::new(),
                data: String::new(),
            },
            Event {
                name: String::new(),
                data: "last".into(),
            },
        ];
        for split in 0..=source.len() {
            let mut decoder = Decoder::default();
            let mut events = Vec::new();
            for chunk in [&source.as_bytes()[..split], &source.as_bytes()[split..]] {
                let mut bytes = Bytes::copy_from_slice(chunk);
                while let Some(event) = decoder.next(&mut bytes).unwrap() {
                    events.push(event);
                }
            }
            assert_eq!(events, expected, "split {split}");
        }
        let mut decoder = Decoder::default();
        let mut events = Vec::new();
        for byte in source.bytes() {
            let mut bytes = Bytes::from(vec![byte]);
            while let Some(event) = decoder.next(&mut bytes).unwrap() {
                events.push(event);
            }
        }
        assert_eq!(events, expected);
    }

    #[test]
    fn invalid_utf8_and_unterminated_events_are_not_accepted() {
        let mut decoder = Decoder::default();
        assert_eq!(
            decoder
                .next(&mut Bytes::from_static(b"data: \xff\n"))
                .unwrap_err()
                .kind,
            ErrorKind::Decode
        );
        let mut decoder = Decoder::default();
        assert!(
            decoder
                .next(&mut Bytes::from_static(b"data: incomplete\n"))
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn stalled_stream_is_network_error() {
        let mut events = Events {
            stream: Box::pin(futures::stream::pending()),
            decoder: Decoder::default(),
            bytes: Bytes::new(),
            idle: Duration::from_millis(1),
        };
        assert_eq!(events.next().await.unwrap_err().kind, ErrorKind::Network);
    }
}
