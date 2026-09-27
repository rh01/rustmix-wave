//! Chapter shard decode.
//!
//! Shards are obfuscated, not DRM: a 32-character MD5 prefix, a dropped lead
//! character, a tail-derived character swap, then base64url. The steps match
//! weread.koplugin `content.lua` and eego `weread_crypto.cpp`.

use crate::weread::crypto::md5_hex;

pub fn decode_shards(shards: &[&str]) -> Result<Vec<u8>, &'static str> {
    let mut payload = String::new();
    for shard in shards {
        if shard.len() <= 32 {
            continue;
        }
        let (prefix, body) = shard.split_at(32);
        let actual = md5_hex(body.as_bytes()).to_ascii_uppercase();
        if !prefix.eq_ignore_ascii_case(&actual) {
            continue;
        }
        payload.push_str(body);
    }
    if payload.len() < 2 {
        return Err("chapter shard was empty or failed its checksum");
    }
    let encoded = &payload[1..];
    let restored = apply_swaps(encoded, &swap_positions(encoded), true);
    base64url_decode(&restored)
}

fn swap_positions(encoded: &str) -> Vec<usize> {
    let bytes = encoded.as_bytes();
    let length = bytes.len();
    if length < 4 {
        return Vec::new();
    }
    if length < 11 {
        return vec![0, 2];
    }
    let n = 4.min((length + 9) / 10);
    let mut tmp = String::new();
    for index in (length - n..length).rev() {
        let value = quart_from_bits(bytes[index]);
        tmp.push_str(&value.to_string());
    }
    let m = length - n - 2;
    if m == 0 {
        return Vec::new();
    }
    let step = m.to_string().len();
    let tmp_bytes = tmp.as_bytes();
    let mut result = Vec::new();
    let mut index = 0usize;
    while result.len() < 10 && index + step < tmp_bytes.len() {
        let first = parse_digits(&tmp, index, step) % m;
        let second = parse_digits(&tmp, index + 1, step) % m;
        result.push(first);
        result.push(second);
        index += step;
    }
    result
}

/// `forward` matches eego `reverse_swaps`: pairs from the tail, offset 1 then 0.
/// Encoding uses the reverse pair and offset order. The tail that feeds
/// [`swap_positions`] is left untouched, so the same positions undo the shuffle.
fn apply_swaps(encoded: &str, positions: &[usize], forward: bool) -> String {
    let mut chars: Vec<u8> = encoded.as_bytes().to_vec();
    let mut pairs = Vec::new();
    let mut index = positions.len() as isize - 1;
    while index > 0 {
        pairs.push(index as usize);
        index -= 2;
    }
    if !forward {
        pairs.reverse();
    }
    for pair in pairs {
        let left_base = positions[pair];
        let right_base = positions[pair - 1];
        let offsets = if forward { [1usize, 0] } else { [0usize, 1] };
        for offset in offsets {
            let left = left_base.saturating_add(offset);
            let right = right_base.saturating_add(offset);
            if left < chars.len() && right < chars.len() {
                chars.swap(left, right);
            }
        }
    }
    String::from_utf8_lossy(&chars).into_owned()
}

fn quart_from_bits(value: u8) -> u32 {
    let mut out = 0u32;
    for bit in 0..8 {
        if (value >> bit) & 1 == 1 {
            out += 1u32 << (2 * bit);
        }
    }
    out
}

fn parse_digits(text: &str, start: usize, step: usize) -> usize {
    let end = (start + step).min(text.len());
    if start >= text.len() || start >= end {
        return 0;
    }
    text.get(start..end)
        .and_then(|slice| slice.parse::<usize>().ok())
        .unwrap_or(0)
}

fn base64url_decode(text: &str) -> Result<Vec<u8>, &'static str> {
    let mut alphabet = String::new();
    for ch in text.chars() {
        match ch {
            '-' => alphabet.push('+'),
            '_' => alphabet.push('/'),
            'A'..='Z' | 'a'..='z' | '0'..='9' | '+' | '/' => alphabet.push(ch),
            _ => {}
        }
    }
    while alphabet.len() % 4 != 0 {
        alphabet.push('=');
    }
    let mut out = Vec::new();
    let bytes = alphabet.as_bytes();
    let mut index = 0usize;
    while index + 4 <= bytes.len() {
        let values = [
            b64_value(bytes[index])?,
            b64_value(bytes[index + 1])?,
            b64_value(bytes[index + 2])?,
            b64_value(bytes[index + 3])?,
        ];
        let triple = (u32::from(values[0]) << 18)
            | (u32::from(values[1]) << 12)
            | (u32::from(values[2]) << 6)
            | u32::from(values[3]);
        out.push((triple >> 16) as u8);
        if bytes[index + 2] != b'=' {
            out.push((triple >> 8) as u8);
        }
        if bytes[index + 3] != b'=' {
            out.push(triple as u8);
        }
        index += 4;
    }
    Ok(out)
}

fn b64_value(byte: u8) -> Result<u8, &'static str> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        b'=' => Ok(0),
        _ => Err("chapter payload is not base64"),
    }
}

pub fn base64url_encode(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let mut index = 0usize;
    while index + 3 <= data.len() {
        let triple = (u32::from(data[index]) << 16)
            | (u32::from(data[index + 1]) << 8)
            | u32::from(data[index + 2]);
        out.push(TABLE[((triple >> 18) & 63) as usize] as char);
        out.push(TABLE[((triple >> 12) & 63) as usize] as char);
        out.push(TABLE[((triple >> 6) & 63) as usize] as char);
        out.push(TABLE[(triple & 63) as usize] as char);
        index += 3;
    }
    if index < data.len() {
        let remain = data.len() - index;
        let mut triple = u32::from(data[index]) << 16;
        if remain == 2 {
            triple |= u32::from(data[index + 1]) << 8;
        }
        out.push(TABLE[((triple >> 18) & 63) as usize] as char);
        out.push(TABLE[((triple >> 12) & 63) as usize] as char);
        if remain == 2 {
            out.push(TABLE[((triple >> 6) & 63) as usize] as char);
        }
    }
    out
}

/// Build one checksummed shard whose decode is `plain`. Test and fixture helper.
#[cfg(test)]
pub fn seal_plain(plain: &str) -> String {
    let encoded = base64url_encode(plain.as_bytes());
    let positions = swap_positions(&encoded);
    let swapped = apply_swaps(&encoded, &positions, false);
    let payload = format!("A{swapped}");
    let prefix = md5_hex(payload.as_bytes()).to_ascii_uppercase();
    format!("{prefix}{payload}")
}

#[cfg(test)]
mod tests {
    use super::{apply_swaps, decode_shards, seal_plain, swap_positions};

    #[test]
    fn swaps_are_involutions_and_leave_the_tail_stable() {
        let sample = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let positions = swap_positions(sample);
        let once = apply_swaps(sample, &positions, true);
        let twice = apply_swaps(&once, &swap_positions(&once), false);
        assert_eq!(twice, sample);
        let n = 4.min((sample.len() + 9) / 10);
        assert_eq!(&once[once.len() - n..], &sample[sample.len() - n..]);
    }

    #[test]
    fn sealed_shard_round_trips_utf8() {
        let plain = "Hello 微信";
        let shard = seal_plain(plain);
        let decoded = decode_shards(&[&shard]).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), plain);
    }

    #[test]
    fn bad_checksum_is_skipped_and_empty_input_fails() {
        assert!(decode_shards(&["zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzznot-the-digest"]).is_err());
        assert!(decode_shards(&["short"]).is_err());
    }
}
