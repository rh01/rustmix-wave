//! IMA ADPCM for short pronunciation clips.
//!
//! The codec stays at 16 kHz PCM16. Each clip is a small `RMXADP1` blob:
//! a 24-byte header plus 4-bit nibbles, low nibble first. One second of
//! speech is about 8 KiB instead of 32 KiB of PCM16. The decoder pulls a
//! bounded chunk so playback can yield back to the UI between I2S writes.

pub const CLIP_MAGIC: &[u8; 8] = b"RMXADP1\0";
pub const CLIP_HEADER_LEN: usize = 24;
pub const CLIP_SAMPLE_RATE_HZ: u32 = 16_000;
/// Twelve seconds at 16 kHz. Longer headers are rejected so a corrupt
/// length cannot stall the main loop.
pub const CLIP_MAX_SAMPLES: u32 = CLIP_SAMPLE_RATE_HZ * 12;
/// Header plus one nibble pair per two samples. A clip longer than this is
/// rejected before any PCM buffer is allocated.
pub const CLIP_MAX_BYTES: u32 = (CLIP_HEADER_LEN as u32) + ((CLIP_MAX_SAMPLES + 1) / 2);

/// On-disk size of one clip, or `None` when `sample_count` is outside the
/// cap or the byte length does not fit in an unsigned integer of `width_bits`
/// (32 on the ESP32).
#[must_use]
pub fn clip_span_bytes(sample_count: u32, width_bits: u32) -> Option<u64> {
    if sample_count == 0 || sample_count > CLIP_MAX_SAMPLES {
        return None;
    }
    let payload = u64::from(sample_count).div_ceil(2);
    let total = u64::from(CLIP_HEADER_LEN as u32).checked_add(payload)?;
    let limit = if width_bits >= 64 {
        u64::MAX
    } else {
        u64::from(u32::MAX)
    };
    if payload > limit || total > limit {
        None
    } else {
        Some(total)
    }
}

const STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

const INDEX_TABLE: [i32; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipHeader {
    pub sample_rate: u32,
    pub sample_count: u32,
    pub predictor: i16,
    pub step_index: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdpcmDecoder {
    predictor: i32,
    step_index: i32,
    samples_left: u32,
}

impl AdpcmDecoder {
    #[must_use]
    pub fn from_header(header: &ClipHeader) -> Self {
        Self {
            predictor: i32::from(header.predictor),
            step_index: i32::from(header.step_index.min(88)),
            samples_left: header.sample_count,
        }
    }

    #[must_use]
    pub const fn samples_left(&self) -> u32 {
        self.samples_left
    }

    #[must_use]
    pub const fn is_done(&self) -> bool {
        self.samples_left == 0
    }

    /// Decode one ADPCM byte (two samples) into PCM16. The final byte of an
    /// odd-length clip contributes only its low nibble.
    pub fn pull_byte(&mut self, byte: u8, output: &mut [i16]) -> usize {
        if output.is_empty() || self.samples_left == 0 {
            return 0;
        }
        output[0] = decode_nibble(byte & 0x0F, &mut self.predictor, &mut self.step_index);
        self.samples_left -= 1;
        if self.samples_left == 0 || output.len() < 2 {
            return 1;
        }
        output[1] = decode_nibble(byte >> 4, &mut self.predictor, &mut self.step_index);
        self.samples_left -= 1;
        2
    }
}

pub fn parse_clip_header(bytes: &[u8]) -> Result<ClipHeader, &'static str> {
    if bytes.len() < CLIP_HEADER_LEN || &bytes[..8] != CLIP_MAGIC {
        return Err("bad pronunciation clip");
    }
    let version = u16::from_le_bytes([bytes[8], bytes[9]]);
    let channels = u16::from_le_bytes([bytes[10], bytes[11]]);
    if version != 1 || channels != 1 {
        return Err("unsupported pronunciation clip");
    }
    let sample_rate = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    let sample_count = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
    if sample_rate != CLIP_SAMPLE_RATE_HZ || sample_count == 0 || sample_count > CLIP_MAX_SAMPLES {
        return Err("pronunciation clip length");
    }
    let predictor = i16::from_le_bytes([bytes[20], bytes[21]]);
    let step_index = bytes[22];
    if step_index > 88 {
        return Err("pronunciation clip step");
    }
    let Some(total) = clip_span_bytes(sample_count, 32) else {
        return Err("pronunciation clip length");
    };
    if bytes.len() as u64 != total {
        return Err("pronunciation clip truncated");
    }
    Ok(ClipHeader {
        sample_rate,
        sample_count,
        predictor,
        step_index,
    })
}

pub fn decode_clip(bytes: &[u8]) -> Result<Vec<i16>, &'static str> {
    let header = parse_clip_header(bytes)?;
    let Some(sample_count) = usize::try_from(header.sample_count).ok() else {
        return Err("pronunciation clip length");
    };
    if sample_count > CLIP_MAX_SAMPLES as usize {
        return Err("pronunciation clip length");
    }
    let mut decoder = AdpcmDecoder::from_header(&header);
    let mut pcm = Vec::with_capacity(sample_count);
    for byte in &bytes[CLIP_HEADER_LEN..] {
        let mut pair = [0_i16; 2];
        let count = decoder.pull_byte(*byte, &mut pair);
        pcm.extend_from_slice(&pair[..count]);
    }
    if !decoder.is_done() || pcm.len() != sample_count {
        return Err("pronunciation clip decode");
    }
    Ok(pcm)
}

/// Encode PCM16 into an `RMXADP1` clip. The pack builder uses the same
/// nibble order so host tests can lock the on-disk bytes.
pub fn encode_clip(samples: &[i16], sample_rate: u32) -> Result<Vec<u8>, &'static str> {
    if sample_rate != CLIP_SAMPLE_RATE_HZ
        || samples.is_empty()
        || samples.len() > CLIP_MAX_SAMPLES as usize
    {
        return Err("pronunciation clip length");
    }
    let mut predictor = 0_i32;
    let mut step_index = 0_i32;
    let mut adpcm = Vec::with_capacity(samples.len().div_ceil(2));
    let mut index = 0;
    while index < samples.len() {
        let low = encode_nibble(samples[index], &mut predictor, &mut step_index);
        let high = if index + 1 < samples.len() {
            encode_nibble(samples[index + 1], &mut predictor, &mut step_index)
        } else {
            0
        };
        adpcm.push(low | (high << 4));
        index += 2;
    }
    let mut bytes = Vec::with_capacity(CLIP_HEADER_LEN + adpcm.len());
    bytes.extend_from_slice(CLIP_MAGIC);
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&0_i16.to_le_bytes());
    bytes.push(0);
    bytes.push(0);
    bytes.extend_from_slice(&adpcm);
    Ok(bytes)
}

fn decode_nibble(nibble: u8, predictor: &mut i32, step_index: &mut i32) -> i16 {
    let nibble = nibble & 0x0F;
    let step = STEP_TABLE[*step_index as usize];
    let mut diff = step >> 3;
    if nibble & 4 != 0 {
        diff += step;
    }
    if nibble & 2 != 0 {
        diff += step >> 1;
    }
    if nibble & 1 != 0 {
        diff += step >> 2;
    }
    if nibble & 8 != 0 {
        *predictor -= diff;
    } else {
        *predictor += diff;
    }
    *predictor = (*predictor).clamp(-32_768, 32_767);
    *step_index = (*step_index + INDEX_TABLE[nibble as usize]).clamp(0, 88);
    *predictor as i16
}

fn encode_nibble(sample: i16, predictor: &mut i32, step_index: &mut i32) -> u8 {
    let step = STEP_TABLE[*step_index as usize];
    let mut diff = i32::from(sample) - *predictor;
    let mut nibble = 0_u8;
    if diff < 0 {
        nibble = 8;
        diff = -diff;
    }
    if diff >= step {
        nibble |= 4;
        diff -= step;
    }
    if diff >= step >> 1 {
        nibble |= 2;
        diff -= step >> 1;
    }
    if diff >= step >> 2 {
        nibble |= 1;
    }
    let _ = decode_nibble(nibble, predictor, step_index);
    nibble
}

#[cfg(test)]
mod tests {
    use super::{
        clip_span_bytes, decode_clip, encode_clip, parse_clip_header, CLIP_HEADER_LEN,
        CLIP_MAX_SAMPLES, CLIP_SAMPLE_RATE_HZ,
    };

    #[test]
    fn round_trip_stays_near_the_source() {
        let source: Vec<i16> = (0..80).map(|index| (index as i16 - 40) * 30).collect();
        let bytes = encode_clip(&source, CLIP_SAMPLE_RATE_HZ).unwrap();
        let decoded = decode_clip(&bytes).unwrap();
        assert_eq!(decoded.len(), source.len());
        let max_error = source
            .iter()
            .zip(decoded.iter())
            .map(|(left, right)| (i32::from(*left) - i32::from(*right)).abs())
            .max()
            .unwrap_or(0);
        assert!(max_error < 4_000, "adpcm error {max_error}");
        let again = encode_clip(&decoded, CLIP_SAMPLE_RATE_HZ).unwrap();
        assert_eq!(bytes, again);
    }

    #[test]
    fn locked_nibble_bytes() {
        let bytes = encode_clip(&[0, 1000, -1000, 0], CLIP_SAMPLE_RATE_HZ).unwrap();
        assert_eq!(&bytes[CLIP_HEADER_LEN..], &[0x70, 0x2F]);
        let header = parse_clip_header(&bytes).unwrap();
        assert_eq!(header.sample_count, 4);
        assert_eq!(header.predictor, 0);
        assert_eq!(header.step_index, 0);
    }

    #[test]
    fn truncated_and_bad_magic_fail() {
        assert!(parse_clip_header(b"nope").is_err());
        let mut bytes = encode_clip(&[0, 1000], CLIP_SAMPLE_RATE_HZ).unwrap();
        bytes.pop();
        assert!(decode_clip(&bytes).is_err());
        bytes = encode_clip(&[0, 1000], CLIP_SAMPLE_RATE_HZ).unwrap();
        bytes[0] = b'X';
        assert!(decode_clip(&bytes).is_err());
    }

    #[test]
    fn hostile_sample_count_is_rejected_before_allocation() {
        assert_eq!(clip_span_bytes(u32::MAX, 32), None);
        assert_eq!(clip_span_bytes(u32::MAX, 64), None);
        assert_eq!(clip_span_bytes(0, 32), None);
        assert_eq!(clip_span_bytes(CLIP_MAX_SAMPLES + 1, 32), None);
        assert_eq!(
            clip_span_bytes(4, 32),
            Some(u64::from(CLIP_HEADER_LEN as u32) + 2)
        );
        let mut header = [0_u8; CLIP_HEADER_LEN];
        header[..8].copy_from_slice(b"RMXADP1\0");
        header[8..10].copy_from_slice(&1_u16.to_le_bytes());
        header[10..12].copy_from_slice(&1_u16.to_le_bytes());
        header[12..16].copy_from_slice(&CLIP_SAMPLE_RATE_HZ.to_le_bytes());
        header[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        let error = parse_clip_header(&header).unwrap_err();
        assert!(
            error.contains("length") || error.contains("truncated"),
            "{error}"
        );
        assert!(decode_clip(&header).is_err());
        let mut bytes = encode_clip(&[0, 1000, -1000, 0], CLIP_SAMPLE_RATE_HZ).unwrap();
        bytes.truncate(CLIP_HEADER_LEN);
        assert!(parse_clip_header(&bytes).unwrap_err().contains("truncated"));
    }
}
