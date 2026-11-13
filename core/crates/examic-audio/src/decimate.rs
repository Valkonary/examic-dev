//! Integer-ratio decimation of microphone PCM down to the 16 kHz that voice
//! activity detection and speech recognition work at.
//!
//! The capture rates used here are exact multiples of 16 kHz, so a fixed
//! low-pass FIR evaluated once per output sample does the job. That is far
//! cheaper than a general asynchronous resampler and has no ratio drift.

use std::f64::consts::PI;

/// Filter length per unit of decimation factor. At 48 kHz -> 16 kHz this gives
/// 96 taps: flat to about 7 kHz and roughly 65 dB down from 9 kHz.
const TAPS_PER_FACTOR: usize = 32;
const KAISER_BETA: f64 = 6.2;
const MAX_FACTOR: usize = 6;
const I16_SCALE: f32 = 1.0 / 32768.0;

pub struct Decimator {
    factor: usize,
    taps: Box<[f32]>,
    /// Input history stored twice back to back, so the newest `taps.len()`
    /// samples are always one contiguous slice.
    history: Box<[f32]>,
    write: usize,
    until_output: usize,
}

impl Decimator {
    /// Returns `None` unless `input_rate` is `output_rate` times 1 to 6.
    pub fn for_rates(input_rate: u32, output_rate: u32) -> Option<Self> {
        if output_rate == 0 || !input_rate.is_multiple_of(output_rate) {
            return None;
        }
        let factor = (input_rate / output_rate) as usize;
        (1..=MAX_FACTOR)
            .contains(&factor)
            .then(|| Self::new(factor))
    }

    fn new(factor: usize) -> Self {
        let taps = if factor == 1 {
            vec![1.0]
        } else {
            design_lowpass(TAPS_PER_FACTOR * factor, 0.5 / factor as f64)
        };
        Self {
            factor,
            history: vec![0.0; taps.len() * 2].into(),
            taps: taps.into(),
            write: 0,
            until_output: factor,
        }
    }

    /// Consumes `input` and appends the decimated samples, scaled to
    /// `-1.0..1.0`, to `out`. Filter state carries over between calls.
    pub fn process(&mut self, input: &[i16], out: &mut Vec<f32>) {
        if self.factor == 1 {
            out.extend(input.iter().map(|&s| s as f32 * I16_SCALE));
            return;
        }
        let n = self.taps.len();
        for &sample in input {
            let x = sample as f32 * I16_SCALE;
            self.history[self.write] = x;
            self.history[self.write + n] = x;
            self.write = (self.write + 1) % n;
            self.until_output -= 1;
            if self.until_output == 0 {
                self.until_output = self.factor;
                out.push(dot(&self.taps, &self.history[self.write..self.write + n]));
            }
        }
    }
}

/// Dot product with eight independent accumulators. A single running float
/// sum cannot be reordered by the compiler and therefore never vectorises;
/// this shape compiles to SIMD multiply-adds.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0f32; 8];
    let (a_chunks, a_tail) = a.as_chunks::<8>();
    let (b_chunks, b_tail) = b.as_chunks::<8>();
    let tail: f32 = a_tail.iter().zip(b_tail).map(|(x, y)| x * y).sum();
    for (x, y) in a_chunks.iter().zip(b_chunks) {
        for i in 0..8 {
            acc[i] += x[i] * y[i];
        }
    }
    acc.iter().sum::<f32>() + tail
}

/// Kaiser-windowed sinc low-pass with unity gain at DC. `cutoff` is the -6 dB
/// point in cycles per input sample.
fn design_lowpass(len: usize, cutoff: f64) -> Vec<f32> {
    let centre = (len - 1) as f64 / 2.0;
    let mut taps: Vec<f64> = (0..len)
        .map(|i| {
            let t = i as f64 - centre;
            let x = 2.0 * cutoff * t;
            let sinc = if x == 0.0 {
                1.0
            } else {
                (PI * x).sin() / (PI * x)
            };
            let r = t / centre;
            let window = bessel_i0(KAISER_BETA * (1.0 - r * r).max(0.0).sqrt());
            2.0 * cutoff * sinc * window
        })
        .collect();
    let gain: f64 = taps.iter().sum();
    taps.iter_mut().for_each(|t| *t /= gain);
    taps.into_iter().map(|t| t as f32).collect()
}

/// Zeroth-order modified Bessel function of the first kind.
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    let half_squared = x * x / 4.0;
    for k in 1..50 {
        term *= half_squared / (k * k) as f64;
        sum += term;
        if term < sum * 1e-12 {
            break;
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: f64, amplitude: f64, len: usize) -> Vec<i16> {
        (0..len)
            .map(|i| ((2.0 * PI * freq * i as f64 / rate).sin() * amplitude * 32767.0) as i16)
            .collect()
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    /// Gain in dB of a full-scale-relative 0.5 amplitude tone, measured after
    /// the filter has settled.
    fn tone_gain_db(freq: f64) -> f64 {
        let mut d = Decimator::for_rates(48_000, 16_000).unwrap();
        let mut out = Vec::new();
        d.process(&sine(freq, 48_000.0, 0.5, 48_000), &mut out);
        20.0 * (rms(&out[200..]) / (0.5 / 2f64.sqrt())).log10()
    }

    #[test]
    fn accepts_only_integer_multiples() {
        assert!(Decimator::for_rates(16_000, 16_000).is_some());
        assert!(Decimator::for_rates(32_000, 16_000).is_some());
        assert!(Decimator::for_rates(48_000, 16_000).is_some());
        assert!(Decimator::for_rates(44_100, 16_000).is_none());
        assert!(Decimator::for_rates(8_000, 16_000).is_none());
        assert!(Decimator::for_rates(192_000, 16_000).is_none());
    }

    #[test]
    fn produces_one_output_per_three_inputs() {
        let mut d = Decimator::for_rates(48_000, 16_000).unwrap();
        let mut out = Vec::new();
        d.process(&[0; 4800], &mut out);
        assert_eq!(out.len(), 1600);
    }

    #[test]
    fn passes_dc_at_unity_gain() {
        let mut d = Decimator::for_rates(48_000, 16_000).unwrap();
        let mut out = Vec::new();
        d.process(&[16_384; 960], &mut out);
        assert!((out.last().unwrap() - 0.5).abs() < 1e-4);
    }

    #[test]
    fn keeps_the_speech_band() {
        for freq in [200.0, 1_000.0, 3_400.0, 6_500.0] {
            let gain = tone_gain_db(freq);
            assert!(gain.abs() < 0.3, "{freq} Hz: {gain:.2} dB");
        }
    }

    #[test]
    fn rejects_what_would_alias() {
        for freq in [9_500.0, 12_000.0, 15_000.0, 20_000.0] {
            let gain = tone_gain_db(freq);
            assert!(gain < -55.0, "{freq} Hz: {gain:.1} dB");
        }
    }

    #[test]
    fn splitting_the_input_does_not_change_the_output() {
        let input = sine(440.0, 48_000.0, 0.4, 5_000);
        let mut whole = Vec::new();
        Decimator::for_rates(48_000, 16_000)
            .unwrap()
            .process(&input, &mut whole);

        let mut pieces = Vec::new();
        let mut d = Decimator::for_rates(48_000, 16_000).unwrap();
        for chunk in input.chunks(77) {
            d.process(chunk, &mut pieces);
        }
        assert_eq!(whole, pieces);
    }

    #[test]
    fn unity_factor_only_rescales() {
        let mut d = Decimator::for_rates(16_000, 16_000).unwrap();
        let mut out = Vec::new();
        d.process(&[0, 16_384, -32_768], &mut out);
        assert_eq!(out, [0.0, 0.5, -1.0]);
    }
}
