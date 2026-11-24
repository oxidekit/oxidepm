//! Byte-oriented line splitting shared by capture and follow

/// Longest line (in bytes) that is kept whole; longer lines are split
pub(crate) const MAX_LINE_BYTES: usize = 64 * 1024;

/// Splits a byte stream into lines without ever failing on invalid UTF-8
///
/// Lines are decoded lossily and are never longer than `MAX_LINE_BYTES`;
/// a longer line is emitted in several pieces.
#[derive(Default)]
pub(crate) struct LineSplitter {
    buf: Vec<u8>,
}

impl LineSplitter {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of bytes, calling `emit` for every completed line
    pub(crate) fn push(&mut self, chunk: &[u8], emit: &mut impl FnMut(String)) {
        let mut rest = chunk;
        while !rest.is_empty() {
            match rest.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    self.append(&rest[..i], emit);
                    self.emit_line(true, emit);
                    rest = &rest[i + 1..];
                }
                None => {
                    self.append(rest, emit);
                    rest = &[];
                }
            }
        }
    }

    /// Emit any unterminated trailing line
    pub(crate) fn finish(&mut self, emit: &mut impl FnMut(String)) {
        if !self.buf.is_empty() {
            self.emit_line(false, emit);
        }
    }

    /// Drop any unterminated trailing line
    pub(crate) fn clear(&mut self) {
        self.buf.clear();
    }

    fn append(&mut self, mut data: &[u8], emit: &mut impl FnMut(String)) {
        while !data.is_empty() {
            if self.buf.len() >= MAX_LINE_BYTES {
                self.emit_line(false, emit);
            }
            let take = data.len().min(MAX_LINE_BYTES - self.buf.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
        }
    }

    fn emit_line(&mut self, strip_cr: bool, emit: &mut impl FnMut(String)) {
        if strip_cr && self.buf.last() == Some(&b'\r') {
            self.buf.pop();
        }
        let line = String::from_utf8_lossy(&self.buf).into_owned();
        self.buf.clear();
        emit(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(chunks: &[&[u8]], finish: bool) -> Vec<String> {
        let mut s = LineSplitter::new();
        let mut out = Vec::new();
        for c in chunks {
            s.push(c, &mut |l| out.push(l));
        }
        if finish {
            s.finish(&mut |l| out.push(l));
        }
        out
    }

    #[test]
    fn splits_across_chunks_and_strips_cr() {
        let out = split(&[b"ab", b"c\r\nd\n\ne"], true);
        assert_eq!(out, vec!["abc", "d", "", "e"]);
    }

    #[test]
    fn invalid_utf8_is_replaced_not_fatal() {
        let out = split(&[b"a\n\xff\nb\n"], true);
        assert_eq!(out, vec!["a", "\u{fffd}", "b"]);
    }

    #[test]
    fn long_lines_are_split_at_the_cap() {
        let data = vec![b'x'; MAX_LINE_BYTES * 2 + 10];
        let mut with_nl = data.clone();
        with_nl.push(b'\n');
        let out = split(&[&with_nl], true);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].len(), MAX_LINE_BYTES);
        assert_eq!(out[1].len(), MAX_LINE_BYTES);
        assert_eq!(out[2].len(), 10);
    }

    #[test]
    fn line_of_exactly_cap_length_is_not_followed_by_an_empty_line() {
        let mut data = vec![b'x'; MAX_LINE_BYTES];
        data.push(b'\n');
        let out = split(&[&data], true);
        assert_eq!(out.len(), 1);
    }
}
