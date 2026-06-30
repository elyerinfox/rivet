use container::AudioInfo;

use super::audio::PreparedAudio;
use super::splice::{trim_audio, trim_frame};

#[test]
fn trim_frame_is_half_open_exact() {
    // `[start, end)` must be exact even at a non-integer detected fps: a frame
    // whose time is < end_sec is kept (ceil), regardless of rounding.
    // 29.9 fps: 7 s = frame 209.3, so the exclusive end is 210 → frame 209
    // (at 6.99 s) IS kept.
    assert_eq!(trim_frame(Some(7.0), 29.9), Some(210));
    assert_eq!(trim_frame(Some(2.0), 29.9), Some(60)); // ceil(59.8)
    // 30 fps exact boundaries.
    assert_eq!(trim_frame(Some(2.0), 30.0), Some(60));
    assert_eq!(trim_frame(Some(5.0), 30.0), Some(150));
    // Open bound and zero.
    assert_eq!(trim_frame(None, 30.0), None);
    assert_eq!(trim_frame(Some(0.0), 30.0), Some(0));
    // Negative time clamps to 0.
    assert_eq!(trim_frame(Some(-3.0), 30.0), Some(0));
}

#[test]
fn trim_audio_keeps_window_and_concat_appends() {
    // 8 packets, 1000 ticks each, timescale 1000 → one packet per second.
    let info = AudioInfo {
        codec: "opus".into(),
        sample_rate: 48000,
        channels: 2,
        timescale: 1000,
        asc_bytes: Vec::new(),
        codec_private: Vec::new(),
    };
    let mk = |n: usize| PreparedAudio {
        info: info.clone(),
        samples: (0..n).map(|i| (vec![i as u8], 1000u32)).collect(),
        handling: "passthrough".into(),
    };
    let a = mk(8);
    // Trim [2s, 5s) keeps packets starting at t=2,3,4 → indices 2,3,4.
    let t = trim_audio(Some(&a), Some(2.0), Some(5.0)).unwrap();
    assert_eq!(t.samples.len(), 3);
    assert_eq!(t.samples[0].0, vec![2u8]);
    assert_eq!(t.samples[2].0, vec![4u8]);
    // Open start keeps from 0; open end keeps to the end.
    assert_eq!(trim_audio(Some(&a), None, Some(3.0)).unwrap().samples.len(), 3);
    assert_eq!(trim_audio(Some(&a), Some(6.0), None).unwrap().samples.len(), 2);
    // No bounds → unchanged.
    assert_eq!(trim_audio(Some(&a), None, None).unwrap().samples.len(), 8);
    // Concat appends.
    let mut joined = mk(3);
    joined.extend(&mk(2));
    assert_eq!(joined.samples.len(), 5);
}
