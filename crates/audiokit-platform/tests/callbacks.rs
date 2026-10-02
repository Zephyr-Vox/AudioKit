//! Production callback allocation guard and actual-consumption/reference regression.
#![cfg(feature = "native-cpal")]
use audiokit::{AudioFormat, ChannelLayout};
use audiokit_platform::{
    cpal::{capture_callback, capture_callback_at, playback_callback},
    ports::*,
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

struct Allocator;
thread_local! { static TRACK: Cell<bool> = const { Cell::new(false) }; static OPS: Cell<usize> = const { Cell::new(0) }; }
fn note() {
    TRACK.with(|v| {
        if v.get() {
            OPS.with(|n| n.set(n.get() + 1));
        }
    });
}
// SAFETY: all operations forward unchanged pointers/layouts to the System allocator.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note();
        unsafe {
            System.dealloc(ptr, layout);
        }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, size) }
    }
}
#[global_allocator]
static ALLOC: Allocator = Allocator;
fn guarded(f: impl FnOnce()) {
    OPS.with(|n| n.set(0));
    TRACK.with(|v| v.set(true));
    f();
    TRACK.with(|v| v.set(false));
    assert_eq!(
        OPS.with(Cell::get),
        0,
        "sample callback allocated/freed/reallocated"
    );
}
fn stereo() -> AudioFormat {
    AudioFormat::new(48_000, ChannelLayout::Stereo).unwrap()
}

#[test]
fn capture_copy_overflow_and_invalid_sample_paths_are_allocation_free() {
    let (mut ingress, mut reader) = capture_pair(stereo(), 4).unwrap();
    let telemetry = ingress.telemetry();
    let data = [0.25, -0.25, 0.5, -0.5, 0.75, -0.75, 0.1, -0.1, 0.2, -0.2];
    guarded(|| capture_callback(&data, &mut ingress, Some(1_000_000)));
    assert_eq!(telemetry.snapshot().dropped_frames, 1);
    for cursor in 0..4 {
        assert_eq!(reader.pop().unwrap().sample_position, cursor);
    }
    guarded(|| capture_callback(&[f32::NAN, 0.0], &mut ingress, None));
    guarded(|| capture_callback(&[0.3, 0.4], &mut ingress, None));
    assert_eq!(reader.pop().unwrap().sample_position, 6);
    assert_eq!(telemetry.snapshot().device_frames, 7);
}

#[test]
fn metadata_and_zero_capacity_stop_errors_are_explicit() {
    assert!(capture_pair(stereo(), 0).is_err());
    assert!(playback_pair(stereo(), 0, 0).is_err());
    let (mut ingress, mut reader) = capture_pair(stereo(), 4).unwrap();
    guarded(|| capture_callback_at(&[0.2_f32, 0.3], &mut ingress, Some(123), Some(456)));
    let frame = reader.pop().unwrap();
    assert_eq!(frame.device_timestamp_ns, Some(123));
    assert_eq!(frame.host_handoff_ns, Some(456));
    let flags = CaptureFlags {
        silent: true,
        discontinuity: true,
        timestamp_error: true,
    };
    guarded(|| {
        ingress.begin_callback();
        ingress.set_device_position(99);
        ingress.push(&[0.0, 0.0], None, flags);
    });
    let frame = reader.pop().unwrap();
    assert_eq!(frame.sample_position, 99);
    assert_eq!(frame.flags, flags);
    let (mut writer, _egress, _reference) = playback_pair(stereo(), 4, 0).unwrap();
    writer.telemetry().stop();
    assert!(writer.write(&[0.1, 0.1]).is_err());
}

#[test]
fn actual_reference_contains_quantized_output_startup_and_underrun_silence() {
    let (mut writer, mut egress, mut reference) = playback_pair(stereo(), 16, 2).unwrap();
    let telemetry = writer.telemetry();
    let mut output = [1_i16; 4];
    guarded(|| playback_callback(&mut output, &mut egress, Some(0)));
    assert_eq!(output, [0; 4]);
    assert!(reference.pop().unwrap().flags.silent);
    assert!(reference.pop().unwrap().flags.silent);
    writer.write(&[0.12345, -0.54321, 0.9, -0.9]).unwrap();
    guarded(|| playback_callback(&mut output, &mut egress, Some(50_000)));
    let first = reference.pop().unwrap();
    assert_eq!(first.samples[0], f32::from(output[0]) / 32768.0);
    assert_eq!(first.samples[1], f32::from(output[1]) / 32768.0);
    reference.pop();
    guarded(|| playback_callback(&mut output, &mut egress, None));
    assert_eq!(output, [0; 4]);
    let silence = reference.pop().unwrap();
    assert!(silence.flags.silent);
    assert_eq!(silence.sample_position, 4);
    assert_eq!(telemetry.snapshot().startup_frames, 2);
    assert_eq!(telemetry.snapshot().underrun_frames, 2);
}

#[test]
fn reference_overflow_cannot_backpressure_playback() {
    let (mut writer, mut egress, _reference) = playback_pair(stereo(), 4, 0).unwrap();
    let telemetry = writer.telemetry();
    let mut output = [0_f32; 8];
    for _ in 0..10 {
        assert_eq!(writer.write(&[0.2; 8]).unwrap().get(), 4);
        guarded(|| playback_callback(&mut output, &mut egress, None));
        assert_eq!(output, [0.2; 8]);
    }
    assert_eq!(telemetry.snapshot().reference_dropped_frames, 36);
    assert_eq!(telemetry.snapshot().underrun_frames, 0);
}
