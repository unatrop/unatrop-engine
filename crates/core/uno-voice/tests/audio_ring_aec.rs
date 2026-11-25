use uno_voice::{
    AecProbe, AecState, AudioFrame, AudioRing, DisabledWakeWord, NoAecProbe, WakeWordDetector,
    rms_level, to_mono_f32_16khz,
};

#[test]
fn missing_probe_reports_unavailable() {
    assert_eq!(NoAecProbe.probe(), AecState::Unavailable);
    assert!(!NoAecProbe.probe().is_available());
}

#[test]
fn resamples_48khz_stereo_to_16khz_mono() {
    let mut pcm = Vec::with_capacity(480 * 2);
    for _ in 0..480 {
        pcm.push(16384);
        pcm.push(16384);
    }
    let frame = AudioFrame::new(pcm, 48_000, 2);
    let out = to_mono_f32_16khz(&frame);
    assert_eq!(out.len(), 160);
    assert!((out[0] - 0.5).abs() < 0.001);
    assert!((rms_level(&out) - 0.5).abs() < 0.001);
}

#[test]
fn ring_keeps_only_the_newest_samples() {
    let mut ring = AudioRing::new(4);
    ring.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);

    assert_eq!(ring.len(), 4);
    assert_eq!(ring.written(), 6);
    assert_eq!(ring.oldest(), 2);
    assert_eq!(ring.slice(2, 6), Some(vec![3.0, 4.0, 5.0, 6.0]));
}

#[test]
fn ring_slice_across_wrap_is_ordered() {
    let mut ring = AudioRing::new(4);
    ring.push(&[1.0, 2.0, 3.0]);
    ring.push(&[4.0, 5.0]);

    assert_eq!(ring.slice(1, 5), Some(vec![2.0, 3.0, 4.0, 5.0]));
    assert_eq!(ring.slice(4, 5), Some(vec![5.0]));
}

#[test]
fn ring_rejects_ranges_outside_the_window() {
    let mut ring = AudioRing::new(4);
    ring.push(&[1.0, 2.0, 3.0, 4.0, 5.0]);

    assert_eq!(ring.slice(0, 2), None, "dropped data");
    assert_eq!(ring.slice(3, 9), None, "beyond newest");
    assert_eq!(ring.slice(3, 3), None, "empty range");
}

#[test]
fn ring_window_before_handles_partial_history() {
    let mut ring = AudioRing::new(16_000);
    ring.push(&[1.0; 800]);

    assert_eq!(ring.window_before(800, 300).len(), 800);
    assert!(ring.window_before(0, 300).is_empty());
}

#[test]
fn ring_clear_keeps_the_absolute_counter() {
    let mut ring = AudioRing::with_seconds(1.0);
    ring.push(&[1.0; 100]);
    ring.clear();

    assert!(ring.is_empty());
    assert_eq!(ring.written(), 100);
    assert_eq!(ring.oldest(), 100);
    assert_eq!(ring.slice(0, 100), None);
}

#[test]
fn ring_reports_buffered_duration() {
    let mut ring = AudioRing::with_seconds(10.0);
    ring.push(&[0.0; 16_000]);
    assert_eq!(ring.buffered_ms(), 1_000);
}

#[test]
fn disabled_detector_never_fires() {
    let mut detector = DisabledWakeWord;
    assert_eq!(detector.push(&[0.1; 1600]), None);
    detector.reset();
}
