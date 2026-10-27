//! 音声PCMの抽出・正規化・変換ヘルパー
//!
//! ScreenCaptureKit の `CMSampleBuffer` から PCM を取り出してモノラル float32
//! に正規化し、ASR 送信用の 24kHz PCM16 へ変換する。

use screencapturekit::prelude::*;

/// キャプチャした音声1バッファ分（モノラル float32 に正規化済み）
pub struct AudioFrame {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u32,
    pub rms: f32,
}

/// CMSampleBuffer から PCM を取り出してモノラル float32 に正規化する
pub fn extract_audio_frame(sample: &CMSampleBuffer) -> Result<AudioFrame, String> {
    let desc = sample
        .format_description()
        .ok_or_else(|| "format descriptionなし".to_string())?;
    let sample_rate = desc
        .audio_sample_rate()
        .ok_or_else(|| "sample rate不明".to_string())? as u32;
    let channels = desc
        .audio_channel_count()
        .ok_or_else(|| "channel数不明".to_string())?;
    let bits = desc
        .audio_bits_per_channel()
        .ok_or_else(|| "bits per channel不明".to_string())?;
    let is_float = desc.audio_is_float();
    if desc.audio_is_big_endian() {
        return Err("big-endian PCM".to_string());
    }

    let abl = sample
        .audio_buffer_list()
        .map_err(|status| format!("AudioBufferList取得失敗: {status}"))?;
    if abl.num_buffers() == 0 {
        return Err("audio bufferなし".to_string());
    }

    let mut per_buffer_mono = Vec::with_capacity(abl.num_buffers());
    for buf in &abl {
        per_buffer_mono.push(decode_buffer(buf.data(), buf.number_channels, bits, is_float)?);
    }
    let mixed = mix_to_mono(&per_buffer_mono);
    let rms = compute_rms(&mixed);
    Ok(AudioFrame {
        samples: mixed,
        sample_rate,
        channels,
        rms,
    })
}

/// 1つの AudioBuffer（インターリーブの可能性あり）をモノラルにデコード
fn decode_buffer(
    bytes: &[u8],
    channels_in_buffer: u32,
    bits: u32,
    is_float: bool,
) -> Result<Vec<f32>, String> {
    if channels_in_buffer == 0 {
        return Err("channel数0のバッファ".to_string());
    }
    let flat: Vec<f32> = match (is_float, bits) {
        (true, 32) => f32le_bytes_to_samples(bytes),
        (false, 16) => i16le_bytes_to_samples(bytes),
        _ => return Err(format!("未対応PCM float={is_float} bits={bits}")),
    };
    Ok(deinterleave_to_mono(&flat, channels_in_buffer as usize))
}

fn f32le_bytes_to_samples(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn i16le_bytes_to_samples(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
        .collect()
}

/// インターリーブ [L1,R1,L2,R2,...] をフレーム毎の平均でモノラル化
fn deinterleave_to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    let frames = interleaved.len() / channels;
    (0..frames)
        .map(|i| {
            let mut sum = 0.0;
            for c in 0..channels {
                sum += interleaved[i * channels + c];
            }
            sum / channels as f32
        })
        .collect()
}

/// ノンインターリーブの各バッファ（各ch）を平均してモノラル化
fn mix_to_mono(buffers: &[Vec<f32>]) -> Vec<f32> {
    if buffers.is_empty() {
        return Vec::new();
    }
    if buffers.len() == 1 {
        return buffers[0].clone();
    }
    let len = buffers.iter().map(|b| b.len()).min().unwrap_or(0);
    (0..len)
        .map(|i| buffers.iter().map(|b| b[i]).sum::<f32>() / buffers.len() as f32)
        .collect()
}

pub fn compute_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

/// RMS → -60〜0dB のレベルメーター文字列
pub fn level_meter(rms: f32, width: usize) -> String {
    let db = rms_to_db(rms);
    let filled = ((db + 60.0) / 60.0 * width as f32).round() as usize;
    "▓".repeat(filled) + &"░".repeat(width - filled)
}

pub fn rms_to_db(rms: f32) -> f32 {
    if rms < 1e-6 {
        -60.0
    } else {
        (20.0 * rms.log10()).clamp(-60.0, 0.0)
    }
}

pub fn format_audio_status(
    sample_rate: u32,
    channels: u32,
    num_samples: usize,
    rms: f32,
    count: usize,
) -> String {
    format!(
        "🎤 {sample_rate}Hz {channels}ch | #{count} {num_samples} samples | {} {:.0}dB",
        level_meter(rms, 10),
        rms_to_db(rms)
    )
}

/// 48kHz → 24kHz ダウンサンプル（2:1平均）
pub fn resample_48k_to_24k(samples: &[f32]) -> Vec<f32> {
    samples
        .chunks_exact(2)
        .map(|c| (c[0] + c[1]) * 0.5)
        .collect()
}

/// float32（-1.0〜1.0）→ PCM16LEバイト列。範囲外はクランプする
pub fn f32_to_pcm16le_bytes(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32768.0)
            .round()
            .clamp(-32768.0, 32767.0) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn f32le_roundtrip() {
        let expected: [f32; 4] = [0.5, -0.25, 1.0, 0.0];
        let mut bytes = Vec::new();
        for v in expected {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let got = f32le_bytes_to_samples(&bytes);
        assert_eq!(got.len(), expected.len());
        for (g, e) in got.iter().zip(expected) {
            assert!(approx_eq(*g, e), "{g} != {e}");
        }
    }

    #[test]
    fn i16le_converts_to_normalized_f32() {
        let bytes = [0x00, 0x00, 0xFF, 0x7F, 0x00, 0x80, 0x00, 0x40];
        let got = i16le_bytes_to_samples(&bytes);
        assert_eq!(got.len(), 4);
        assert!(approx_eq(got[0], 0.0));
        assert!(approx_eq(got[1], 32767.0 / 32768.0));
        assert!(approx_eq(got[2], -1.0));
        assert!(approx_eq(got[3], 0.5));
    }

    #[test]
    fn deinterleave_stereo_averages_frames() {
        let got = deinterleave_to_mono(&[1.0, -1.0, 0.5, 0.5], 2);
        assert_eq!(got.len(), 2);
        assert!(approx_eq(got[0], 0.0));
        assert!(approx_eq(got[1], 0.5));
    }

    #[test]
    fn mix_to_mono_averages_and_truncates() {
        let got = mix_to_mono(&[vec![1.0, 2.0, 99.0], vec![3.0, 4.0]]);
        assert_eq!(got.len(), 2);
        assert!(approx_eq(got[0], 2.0));
        assert!(approx_eq(got[1], 3.0));
    }

    #[test]
    fn rms_of_known_signal() {
        let got = compute_rms(&[3.0, 4.0]);
        assert!((got - 12.5_f32.sqrt()).abs() < 1e-5, "{got}");
        assert_eq!(compute_rms(&[]), 0.0);
    }

    #[test]
    fn meter_full_on_max_and_empty_on_silence() {
        assert_eq!(level_meter(1.0, 10).chars().filter(|&c| c == '▓').count(), 10);
        assert_eq!(level_meter(0.0, 10).chars().filter(|&c| c == '▓').count(), 0);
        assert_eq!(level_meter(1.0, 10).chars().count(), 10);
    }

    #[test]
    fn status_contains_format_and_level() {
        let s = format_audio_status(48_000, 2, 1024, 0.5, 21);
        assert!(s.contains("48000Hz"), "{s}");
        assert!(s.contains("2ch"), "{s}");
        assert!(s.contains("#21"), "{s}");
        assert!(s.contains("1024 samples"), "{s}");
    }

    #[test]
    fn decode_rejects_unsupported_pcm() {
        assert!(decode_buffer(&[0u8; 8], 2, 24, false).is_err());
        assert!(decode_buffer(&[0u8; 8], 0, 16, false).is_err());
    }

    #[test]
    fn resample_averages_pairs() {
        let got = resample_48k_to_24k(&[1.0, 3.0, 0.5, -0.5, 9.0]);
        assert_eq!(got.len(), 2);
        assert!(approx_eq(got[0], 2.0));
        assert!(approx_eq(got[1], 0.0));
    }

    #[test]
    fn pcm16_quantizes_and_clamps() {
        let got = f32_to_pcm16le_bytes(&[0.0, 1.0, -1.0, 2.0, -2.0, 0.5]);
        let words: Vec<i16> = got
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(words, vec![0, 32767, -32768, 32767, -32768, 16384]);
    }
}
