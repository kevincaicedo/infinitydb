//! Incremental framing: each header byte and completed item is visited once.

pub const MAX_DEPTH: usize = 32;
pub const MAX_BYTES: usize = 1024 * 1024;
pub const MAX_ITEMS: usize = 64 * 1024;
const MAX_LINE_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Malformed,
    DepthLimit,
    ByteLimit,
    ItemLimit,
    LineLimit,
}

impl std::fmt::Display for Error {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(match self {
            Self::Malformed => "malformed RESP reply",
            Self::DepthLimit => "RESP nesting limit exceeded",
            Self::ByteLimit => "RESP byte limit exceeded",
            Self::ItemLimit => "RESP item limit exceeded",
            Self::LineLimit => "RESP line limit exceeded",
        })
    }
}

#[derive(Default)]
pub struct Decoder {
    pending: [usize; MAX_DEPTH],
    depth: usize,
    position: usize,
    scanned: usize,
    items: usize,
    bulk_end: Option<usize>,
    complete: Option<usize>,
    error: Option<Error>,
}

impl Decoder {
    /// The buffer must retain its prefix between calls. Errors are terminal.
    pub fn advance(&mut self, input: &[u8]) -> Result<Option<usize>, Error> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if let Some(end) = self.complete {
            return Ok(Some(end));
        }
        let result = self.walk(&input[..input.len().min(MAX_BYTES)]);
        let result = match result {
            Ok(None) if input.len() >= MAX_BYTES => Err(Error::ByteLimit),
            result => result,
        };
        match result {
            Ok(end) => self.complete = end,
            Err(error) => self.error = Some(error),
        }
        result
    }

    fn walk(&mut self, input: &[u8]) -> Result<Option<usize>, Error> {
        loop {
            if let Some(end) = self.bulk_end {
                if input.len() < end {
                    return Ok(None);
                }
                if &input[end - 2..end] != b"\r\n" {
                    return Err(Error::Malformed);
                }
                self.position = end;
                self.bulk_end = None;
                if self.finish_item() {
                    return Ok(Some(end));
                }
            }
            let Some(&tag) = input.get(self.position) else { return Ok(None) };
            if !b"+-:,#(_$=!*%~>".contains(&tag) {
                return Err(Error::Malformed);
            }
            let Some(end) = self.line_end(input)? else { return Ok(None) };
            let body = &input[self.position + 1..end - 2];
            self.position = end;
            self.items += 1;
            if self.items > MAX_ITEMS {
                return Err(Error::ItemLimit);
            }
            let children = self.header(tag, body)?;
            if self.bulk_end.is_some() {
                continue;
            }
            if let Some(children) = children {
                if self.depth == MAX_DEPTH {
                    return Err(Error::DepthLimit);
                }
                if children > 0 {
                    self.pending[self.depth] = children;
                    self.depth += 1;
                    continue;
                }
            }
            if self.finish_item() {
                return Ok(Some(self.position));
            }
        }
    }

    fn header(&mut self, tag: u8, body: &[u8]) -> Result<Option<usize>, Error> {
        match tag {
            b'$' | b'=' | b'!' => {
                let length = length(body, tag == b'$')?;
                if let Some(length) = length {
                    let end = self.position.checked_add(length).and_then(|n| n.checked_add(2));
                    let end = end.filter(|&n| n <= MAX_BYTES).ok_or(Error::ByteLimit)?;
                    self.bulk_end = Some(end);
                }
            }
            b'*' | b'%' | b'~' | b'>' => {
                let count = length(body, tag == b'*')?.unwrap_or(0);
                let count = count.checked_mul(if tag == b'%' { 2 } else { 1 });
                let count = count.filter(|&n| n <= MAX_ITEMS - self.items);
                return Ok(Some(count.ok_or(Error::ItemLimit)?));
            }
            b'_' if !body.is_empty() => return Err(Error::Malformed),
            b'#' if body != b"t" && body != b"f" => return Err(Error::Malformed),
            b':' if std::str::from_utf8(body)
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
                .is_none() =>
            {
                return Err(Error::Malformed);
            }
            _ => {}
        }
        Ok(None)
    }

    fn line_end(&mut self, input: &[u8]) -> Result<Option<usize>, Error> {
        self.scanned = self.scanned.max(self.position + 1);
        while let Some(&byte) = input.get(self.scanned) {
            if self.scanned - self.position >= MAX_LINE_BYTES {
                return Err(Error::LineLimit);
            }
            if byte == b'\n' {
                return Err(Error::Malformed);
            }
            if byte == b'\r' {
                return match input.get(self.scanned + 1) {
                    Some(b'\n') if self.scanned + 2 - self.position <= MAX_LINE_BYTES => {
                        Ok(Some(self.scanned + 2))
                    }
                    Some(b'\n') => Err(Error::LineLimit),
                    Some(_) => Err(Error::Malformed),
                    None => Ok(None),
                };
            }
            self.scanned += 1;
        }
        Ok(None)
    }

    fn finish_item(&mut self) -> bool {
        while self.depth > 0 {
            self.pending[self.depth - 1] -= 1;
            if self.pending[self.depth - 1] > 0 {
                return false;
            }
            self.depth -= 1;
        }
        true
    }
}

fn length(body: &[u8], nullable: bool) -> Result<Option<usize>, Error> {
    if nullable && body == b"-1" {
        return Ok(None);
    }
    if body.is_empty() || !body.iter().all(u8::is_ascii_digit) {
        return Err(Error::Malformed);
    }
    std::str::from_utf8(body)
        .ok()
        .and_then(|text| text.parse().ok())
        .map(Some)
        .ok_or(Error::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_byte_item_line_and_depth_boundaries() {
        let payload = MAX_BYTES - 12; // "$1048564\r\n" + payload + CRLF.
        let frame = format!("${payload}\r\n{}\r\n", "x".repeat(payload));
        assert_eq!(frame.len(), MAX_BYTES);
        assert_eq!(Decoder::default().advance(frame.as_bytes()), Ok(Some(MAX_BYTES)));
        assert_eq!(Decoder::default().advance(b"$1048565\r\n"), Err(Error::ByteLimit));
        let frame = format!("*{}\r\n{}", MAX_ITEMS - 1, ":0\r\n".repeat(MAX_ITEMS - 1));
        assert_eq!(Decoder::default().advance(frame.as_bytes()), Ok(Some(frame.len())));
        assert_eq!(Decoder::default().advance(b"*65536\r\n"), Err(Error::ItemLimit));
        let line = format!("+{}\r\n", "x".repeat(MAX_LINE_BYTES - 3));
        assert_eq!(Decoder::default().advance(line.as_bytes()), Ok(Some(MAX_LINE_BYTES)));
        let line = format!("+{}\r\n", "x".repeat(MAX_LINE_BYTES - 2));
        assert_eq!(Decoder::default().advance(line.as_bytes()), Err(Error::LineLimit));
        let frame = format!("{}*0\r\n", "*1\r\n".repeat(MAX_DEPTH - 1));
        assert_eq!(Decoder::default().advance(frame.as_bytes()), Ok(Some(frame.len())));
    }

    #[test]
    fn fragmented_mixed_aggregates_bulk_and_pipelined_suffix_are_stable() {
        let first = b"%1\r\n+k\r\n*2\r\n$3\r\na\r\n\r\n~0\r\n";
        let mut decoder = Decoder::default();
        for end in 0..first.len() {
            assert_eq!(decoder.advance(&first[..end]), Ok(None), "prefix {end}");
        }
        assert_eq!(decoder.advance(first), Ok(Some(first.len())));
        let with_suffix = [first.as_slice(), b"+OK\r\n"].concat();
        assert_eq!(decoder.advance(&with_suffix), Ok(Some(first.len())));
        assert_eq!(Decoder::default().advance(&with_suffix), Ok(Some(first.len())));
    }

    #[test]
    fn malformed_lengths_trailers_tags_and_terminal_errors() {
        for bytes in [
            b"$-2\r\n".as_slice(),
            b"*-2\r\n",
            b"$+1\r\nx\r\n",
            b"$1\r\nx!!",
            b"?",
            b"+x\ny\r\n",
            b"_x\r\n",
            b"#x\r\n",
            b":x\r\n",
            b"$18446744073709551616\r\n",
        ] {
            let mut decoder = Decoder::default();
            assert_eq!(decoder.advance(bytes), Err(Error::Malformed), "{bytes:?}");
            assert_eq!(decoder.advance(bytes), Err(Error::Malformed));
        }
        for bytes in [b"$-1\r\n".as_slice(), b"*-1\r\n", b"_\r\n", b"#f\r\n"] {
            assert_eq!(Decoder::default().advance(bytes), Ok(Some(bytes.len())));
        }
    }
}
