//! Grow a response buffer only after the size is known to fit.

#[derive(Debug)]
pub struct BoundedBody {
    data: Vec<u8>,
    max: usize,
}

impl BoundedBody {
    pub fn new(content_length: Option<usize>, max: usize) -> Result<Self, &'static str> {
        let capacity = match content_length {
            Some(len) if len > max => return Err("response exceeds size limit"),
            Some(len) => len,
            None => 4 * 1024.min(max),
        };
        let mut data = Vec::new();
        data.try_reserve(capacity)
            .map_err(|_| "response exceeds size limit")?;
        Ok(Self { data, max })
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<(), &'static str> {
        if chunk.len() > self.max || self.data.len().saturating_add(chunk.len()) > self.max {
            return Err("response exceeds size limit");
        }
        self.data
            .try_reserve(chunk.len())
            .map_err(|_| "response exceeds size limit")?;
        self.data.extend_from_slice(chunk);
        Ok(())
    }

    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.data
    }
}

/// A normal end of the connection is a complete body only when every declared
/// byte arrived, and a chunked body only after the terminal chunk.
///
/// `content_length` is negative when the response has no Content-Length.
/// `terminal_chunk` is the ESP-IDF "complete data received" flag: for a
/// chunked response that is the zero-length chunk.
#[must_use]
pub fn body_transfer_error(
    content_length: i64,
    bytes_read: usize,
    chunked: bool,
    terminal_chunk: bool,
) -> Option<&'static str> {
    if !chunked && content_length >= 0 && bytes_read != content_length as usize {
        return Some("HTTP response ended before Content-Length");
    }
    if chunked && !terminal_chunk {
        return Some("HTTP response ended before the terminal chunk");
    }
    None
}

/// Cancel closes the socket. A short read after that is a cancel, not a body
/// that should be stored.
#[must_use]
pub fn stopped_transfer_error(
    cancelled: bool,
    content_length: i64,
    bytes_read: usize,
    chunked: bool,
    terminal_chunk: bool,
) -> Option<&'static str> {
    if cancelled {
        return Some("cancelled");
    }
    body_transfer_error(content_length, bytes_read, chunked, terminal_chunk)
}

#[cfg(test)]
mod tests {
    use super::BoundedBody;

    #[test]
    fn hostile_content_length_allocates_nothing() {
        assert!(BoundedBody::new(Some(usize::MAX), 1024).is_err());
        assert!(BoundedBody::new(Some(1025), 1024).is_err());
        let mut body = BoundedBody::new(None, 8).unwrap();
        body.push(b"abcd").unwrap();
        assert!(body.push(b"efghij").is_err());
        assert_eq!(body.len(), 4);
    }

    #[test]
    fn short_content_length_and_missing_terminal_chunk_are_failures() {
        assert_eq!(super::body_transfer_error(100, 100, false, true), None);
        assert_eq!(
            super::body_transfer_error(100, 40, false, false),
            Some("HTTP response ended before Content-Length")
        );
        assert_eq!(
            super::body_transfer_error(-1, 40, true, false),
            Some("HTTP response ended before the terminal chunk")
        );
        assert_eq!(super::body_transfer_error(-1, 40, true, true), None);
        assert_eq!(
            super::stopped_transfer_error(true, 100, 40, false, false),
            Some("cancelled")
        );
    }
}
