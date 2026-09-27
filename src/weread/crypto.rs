//! WeRead web hashing.
//!
//! `_e` and the `0x15051505` signature follow the web reader as documented by
//! finlater/weread.koplugin and ported in MokuMMk/eego-a4-weread.

use md5::{Digest, Md5};
use sha2::Sha256;

pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/135.0.0.0 Safari/537.36 Edg/135.0.0.0";
pub const READER_TOKEN: &str = "3c5c8717f3daf09iop3423zafeqoi";

#[must_use]
pub fn md5_hex(data: &[u8]) -> String {
    let digest = Md5::digest(data);
    hex_encode(&digest)
}

#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    hex_encode(&digest)
}

#[must_use]
pub fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let c = *byte;
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~') {
            out.push(c as char);
        } else {
            out.push('%');
            out.push(hex_digit_upper(c >> 4));
            out.push(hex_digit_upper(c & 0x0f));
        }
    }
    out
}

/// Deterministic browser app id derived from the User-Agent.
#[must_use]
pub fn web_app_id(user_agent: &str) -> String {
    let mut prefix = String::new();
    let mut count = 0usize;
    for part in user_agent.split_whitespace() {
        if count >= 12 {
            break;
        }
        prefix.push(char::from(b'0' + (part.len() % 10) as u8));
        count += 1;
    }
    let mut hash: u32 = 0;
    for byte in user_agent.bytes() {
        hash = (0x83u32.wrapping_mul(hash).wrapping_add(u32::from(byte))) & 0x7fff_ffff;
    }
    format!("wb{prefix}h{hash}")
}

/// Web reader `_e(value)` encoding for book ids, chapter uids, and timestamps.
#[must_use]
pub fn e_hash(value: &str) -> String {
    let digest = md5_hex(value.as_bytes());
    let mut result = digest[..3].to_string();
    let mut chunks: Vec<String> = Vec::new();
    let type_flag = if is_digit_string(value) {
        let bytes = value.as_bytes();
        let mut index = 0usize;
        while index < bytes.len() {
            let end = (index + 9).min(bytes.len());
            let part = &value[index..end];
            let number = part.parse::<u64>().unwrap_or(0);
            chunks.push(format!("{number:x}"));
            index = end;
        }
        "3"
    } else {
        chunks.push(byte_hex(value.as_bytes()));
        "4"
    };
    result.push_str(type_flag);
    result.push('2');
    result.push_str(&digest[digest.len() - 2..]);
    for (index, chunk) in chunks.iter().enumerate() {
        result.push_str(&format!("{len:02x}", len = chunk.len()));
        result.push_str(chunk);
        if index + 1 < chunks.len() {
            result.push('g');
        }
    }
    if result.len() < 20 {
        let need = 20 - result.len();
        result.push_str(&digest[..need]);
    }
    let suffix = md5_hex(result.as_bytes());
    result.push_str(&suffix[..3]);
    result
}

/// Signature over a sorted `key=value` query. Initial state is `0x15051505`.
#[must_use]
pub fn sign(query: &str) -> String {
    let bytes = query.as_bytes();
    let length = bytes.len();
    let mut a: u64 = 0x1505_1505;
    let mut b = a;
    let mut index = length;
    while index > 1 {
        let shift_a = ((length - index + 1) % 30) as u32;
        let shift_b = ((index - 1) % 30) as u32;
        let current = u64::from(bytes[index - 1]);
        let previous = u64::from(bytes[index - 2]);
        a = (a ^ (current << shift_a)) & 0x7fff_ffff;
        b = (b ^ (previous << shift_b)) & 0x7fff_ffff;
        index -= 2;
    }
    format!("{sum:x}", sum = a.saturating_add(b))
}

#[must_use]
pub fn sorted_query(fields: &[(&str, &str)]) -> String {
    let mut keys: Vec<&str> = fields
        .iter()
        .map(|(key, _)| *key)
        .filter(|key| *key != "s")
        .collect();
    keys.sort_unstable();
    keys.dedup();
    let mut query = String::new();
    for key in keys {
        let value = fields
            .iter()
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, value)| *value)
            .unwrap_or("");
        if !query.is_empty() {
            query.push('&');
        }
        query.push_str(key);
        query.push('=');
        query.push_str(&url_encode(value));
    }
    query
}

fn is_digit_string(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn byte_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:x}"));
    }
    out
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(hex_digit(byte >> 4));
        out.push(hex_digit(byte & 0x0f));
    }
    out
}

fn hex_digit(value: u8) -> char {
    b"0123456789abcdef"[usize::from(value)] as char
}

fn hex_digit_upper(value: u8) -> char {
    b"0123456789ABCDEF"[usize::from(value)] as char
}

#[cfg(test)]
mod tests {
    use super::{
        e_hash, md5_hex, sha256_hex, sign, sorted_query, url_encode, web_app_id, USER_AGENT,
    };

    #[test]
    fn md5_and_sha256_match_known_digests() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"43208843").len(), 32);
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn encodes_documented_book_and_chapter_ids() {
        assert_eq!(e_hash("43208843"), "c9c321c07293508bc9c79df");
        assert_eq!(e_hash("2"), "c81322c012c81e728d9d180");
        assert_eq!(e_hash("119"), "07e323f027707e1cd7dc674");
    }

    #[test]
    fn signature_is_stable_and_order_independent() {
        let fields = [("b", "abc"), ("a", "1"), ("prevChapter", "false")];
        let reversed = [("prevChapter", "false"), ("b", "abc"), ("a", "1")];
        assert_eq!(sorted_query(&fields), sorted_query(&reversed));
        assert_eq!(sorted_query(&fields), "a=1&b=abc&prevChapter=false");
        let signature = sign("a=1&b=abc&prevChapter=false");
        assert_eq!(signature, sign(sorted_query(&fields).as_str()));
        assert!(signature.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_eq!(sign(""), format!("{:x}", 0x1505_1505u64 * 2));
    }

    #[test]
    fn url_encode_matches_encode_uri_component_bytes() {
        assert_eq!(url_encode("a b"), "a%20b");
        assert_eq!(url_encode("a-b_c.d~"), "a-b_c.d~");
        assert_eq!(url_encode("你"), "%E4%BD%A0");
    }

    #[test]
    fn web_app_id_uses_token_lengths_and_hash() {
        let id = web_app_id("ab cd");
        assert!(id.starts_with("wb22h"));
        assert_eq!(web_app_id(USER_AGENT), web_app_id(USER_AGENT));
        assert!(web_app_id(USER_AGENT).starts_with("wb"));
    }
}
