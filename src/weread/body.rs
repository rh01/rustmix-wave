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
}
