//! Captures PipeWire video (negotiated via [`crate::screencast`]) and,
//! optionally, desktop audio (a direct, non-portal-gated connection to the
//! default sink's monitor - `prtsc` isn't sandboxed, so no permission gate
//! applies), encodes them with `openh264`/`fdk-aac`, and muxes both to one
//! MP4 - all on one dedicated thread, since PipeWire's mainloop is
//! blocking, not async.

use std::cell::RefCell;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use fdk_aac::enc as aac;
use mp4::{AacConfig, AvcConfig, ChannelConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig};
use openh264::OpenH264API;
use openh264::encoder::{Encoder, EncoderConfig};
use openh264::formats::{BgraSliceU8, RgbaSliceU8, YUVBuffer};
use pipewire as pw;
use pw::spa::param::audio::AudioFormat;
use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Pod, Value, object, property};
use pw::spa::utils::{Direction, SpaTypes};
use pw::stream::StreamFlags;

/// MP4 timescale: units per second used for sample timestamps/durations.
const TIMESCALE: u32 = 1000;
/// Minimum time between processed video frames (see the video `process`
/// callback for why this matters beyond just capping the output frame
/// rate).
const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(1000 / 30);
/// Audio sample rate requested from PipeWire and configured on the AAC
/// encoder - PipeWire's internal graph commonly already runs at 48kHz, so
/// this avoids relying on PipeWire's own (unverified, for our purposes)
/// resampling.
const AUDIO_SAMPLE_RATE: u32 = 48000;
/// Fixed at stereo - matches typical desktop audio, and the `fdk-aac`
/// binding's `Encoder` is hardcoded to allocate 2 channels internally
/// regardless of `ChannelMode` (see its own source).
const AUDIO_CHANNELS: u32 = 2;
/// AAC-LC encodes exactly this many samples *per channel* per frame.
const AAC_FRAME_SAMPLES: usize = 1024;

/// Which interleaved pixel layout PipeWire negotiated - both map directly
/// onto an `openh264` slice-wrapper type, so no manual RGB/BGR channel
/// swapping is needed, just picking the matching wrapper at encode time.
#[derive(Clone, Copy)]
enum PixelLayout {
    Bgra,
    Rgba,
}

/// Mutable state shared between the video and (optional) audio streams'
/// `param_changed`/`process` callbacks. All of it - both streams included -
/// runs on the single PipeWire mainloop thread, so a plain `RefCell` (no
/// `Mutex`) is enough.
struct EncodeState {
    writer: Mp4Writer<File>,
    /// `Mp4Writer::add_track` assigns ids sequentially starting at 1, with
    /// no getter to read that count back - tracked here instead so video's
    /// and audio's tracks (added lazily, independently, in whichever order
    /// their first usable data happens to arrive) each get a stable,
    /// correct id to write samples against afterward.
    next_track_id: u32,
    /// Wall-clock start of the recording, set by whichever of video/audio
    /// starts first - both compute their own per-sample timestamps as
    /// elapsed time since this.
    start: Option<Instant>,
    error: Option<String>,

    encoder: Encoder,
    layout: Option<PixelLayout>,
    size: (usize, usize),
    video_track_id: Option<u32>,
    last_frame_at: Option<Instant>,
    frame_count: u32,

    audio: Option<AudioState>,
}

/// Audio-specific encode state, present only when `--audio` was requested.
struct AudioState {
    encoder: aac::Encoder,
    track_id: Option<u32>,
    /// Interleaved S16LE samples accumulated from `process` until there's
    /// enough for one AAC frame (`AAC_FRAME_SAMPLES` per channel).
    pcm: Vec<i16>,
    frame_count: u32,
}

/// Sent through a [`pipewire::channel`] to stop a running [`record`] call
/// from another thread. Used by both the CLI (translating Ctrl-C/SIGTERM,
/// detected via `tokio::signal` on the async side, into this) and the MCP
/// server (translating a `stop_recording` tool call into this) - a single
/// stop mechanism for both, rather than relying on OS signal delivery
/// racing between threads (which real testing found to be genuinely
/// unreliable - see the implementation plan).
pub struct Terminate;

/// Runs the capture+encode+mux loop until a [`Terminate`] message arrives
/// on `stop_rx`, blocking the calling thread. Meant to be driven via
/// `tokio::task::spawn_blocking` from async code.
pub fn record(
    fd: OwnedFd,
    node_id: u32,
    size: (i32, i32),
    output: &Path,
    audio: bool,
    stop_rx: pw::channel::Receiver<Terminate>,
) -> Result<(), String> {
    pw::init();

    let main_loop = pw::main_loop::MainLoopRc::new(None).map_err(|err| err.to_string())?;

    let weak = main_loop.downgrade();
    let _stop_listener = stop_rx.attach(main_loop.loop_(), move |Terminate| {
        if let Some(main_loop) = weak.upgrade() {
            main_loop.quit();
        }
    });

    let context = pw::context::ContextRc::new(&main_loop, None).map_err(|err| err.to_string())?;
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|err| err.to_string())?;

    let file = File::create(output).map_err(|err| err.to_string())?;
    eprintln!("Recording to {}...", output.display());
    let mp4_config = Mp4Config {
        major_brand: str::parse("isom").unwrap(),
        minor_version: 512,
        compatible_brands: vec![
            str::parse("isom").unwrap(),
            str::parse("iso2").unwrap(),
            str::parse("avc1").unwrap(),
            str::parse("mp41").unwrap(),
        ],
        timescale: TIMESCALE,
    };
    let writer = Mp4Writer::write_start(file, &mp4_config).map_err(|err| err.to_string())?;

    let api = OpenH264API::from_source();
    let encoder_config = EncoderConfig::new();
    let encoder = Encoder::with_api_config(api, encoder_config).map_err(|err| err.to_string())?;

    let audio_state = if audio {
        let encoder = aac::Encoder::new(aac::EncoderParams {
            bit_rate: aac::BitRate::Cbr(128_000),
            sample_rate: AUDIO_SAMPLE_RATE,
            transport: aac::Transport::Raw,
            channels: aac::ChannelMode::Stereo,
            audio_object_type: aac::AudioObjectType::Mpeg4LowComplexity,
        })
        .map_err(|err| format!("failed to create AAC encoder: {err}"))?;
        Some(AudioState {
            encoder,
            track_id: None,
            pcm: Vec::new(),
            frame_count: 0,
        })
    } else {
        None
    };

    let state = Rc::new(RefCell::new(EncodeState {
        writer,
        next_track_id: 1,
        start: None,
        error: None,
        encoder,
        layout: None,
        size: (size.0.max(0) as usize, size.1.max(0) as usize),
        video_track_id: None,
        last_frame_at: None,
        frame_count: 0,
        audio: audio_state,
    }));

    let stream = pw::stream::StreamBox::new(
        &core,
        "prtsc-record",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|err| err.to_string())?;

    let param_state = state.clone();
    let process_state = state.clone();
    let _listener = stream
        .add_local_listener_with_user_data(VideoInfoRaw::default())
        .param_changed(move |_stream, format, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = pw::spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
                return;
            }
            if format.parse(param).is_err() {
                return;
            }
            let layout = match format.format() {
                VideoFormat::BGRA | VideoFormat::BGRx => PixelLayout::Bgra,
                VideoFormat::RGBA | VideoFormat::RGBx => PixelLayout::Rgba,
                _ => return,
            };
            let mut state = param_state.borrow_mut();
            state.layout = Some(layout);
            state.size = (format.size().width as usize, format.size().height as usize);
        })
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let stride = data.chunk().stride().max(0) as usize;
            let Some(bytes) = data.data() else { return };

            let mut state = process_state.borrow_mut();
            if state.error.is_some() {
                return;
            }
            // Throttle to MIN_FRAME_INTERVAL regardless of how fast PipeWire
            // delivers buffers or how slow encoding is: without this, a
            // source that can outpace the encoder (observed at 4K, where
            // openh264 can't sustain real time) keeps this thread
            // permanently busy inside encode_frame, starving the event
            // loop's own signal handling - Ctrl-C stopped responding
            // entirely under that load. Dequeuing-and-dropping a frame is
            // cheap, so skipped frames still return to the event loop
            // quickly, giving the pending SIGINT/SIGTERM a chance to be
            // serviced on the very next iteration.
            let due = state
                .last_frame_at
                .is_none_or(|last| last.elapsed() >= MIN_FRAME_INTERVAL);
            if !due {
                return;
            }
            state.last_frame_at = Some(Instant::now());
            let Some(layout) = state.layout else { return };
            if let Err(err) = encode_frame(&mut state, layout, stride, bytes) {
                state.error = Some(err);
            }
        })
        .register()
        .map_err(|err| err.to_string())?;

    let values = video_format_pod(size)?;
    let format_pod = Pod::from_bytes(&values).ok_or("failed to build format pod")?;
    let mut params = [format_pod];
    stream
        .connect(
            Direction::Input,
            Some(node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|err| err.to_string())?;

    // Audio isn't gated by the portal at all - a direct, unsandboxed
    // connection to the *default* PipeWire socket (no fd from the
    // screencast session) rather than the portal-scoped one video uses.
    // Kept on its own Context/Core so a failure connecting for audio can't
    // affect the video stream, but attached to the same main_loop so one
    // thread/event loop (and one Terminate listener) covers both.
    //
    // Uses `StreamRc` rather than video's `StreamBox`: `StreamRc::new`
    // takes an *owned* `CoreRc` and keeps it alive internally, with no
    // lifetime tying the stream to a caller-held reference - `StreamBox`
    // borrows its `Core` instead, which doesn't work here since this
    // stream/listener pair is built inside an `if` and needs to outlive
    // that block.
    let _audio = if audio {
        let audio_context =
            pw::context::ContextRc::new(&main_loop, None).map_err(|err| err.to_string())?;
        let audio_core = audio_context
            .connect_rc(None)
            .map_err(|err| err.to_string())?;
        let audio_stream = pw::stream::StreamRc::new(
            audio_core,
            "prtsc-record-audio",
            pw::properties::properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Production",
                // Route to the default sink's monitor (i.e. "what's
                // currently playing") rather than a microphone input -
                // this is the standard PipeWire idiom for desktop-audio
                // capture (same property `pw-record --target
                // @DEFAULT_SINK@` relies on).
                *pw::keys::STREAM_CAPTURE_SINK => "true",
            },
        )
        .map_err(|err| err.to_string())?;

        let audio_process_state = state.clone();
        let audio_listener = audio_stream
            .add_local_listener_with_user_data(pw::spa::param::audio::AudioInfoRaw::default())
            .param_changed(|_stream, _format, _id, _param| {})
            .process(move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else {
                    return;
                };
                let Some(bytes) = data.data() else { return };

                let mut state = audio_process_state.borrow_mut();
                if state.error.is_some() {
                    return;
                }
                if let Err(err) = encode_audio(&mut state, bytes) {
                    state.error = Some(err);
                }
            })
            .register()
            .map_err(|err| err.to_string())?;

        let values = audio_format_pod()?;
        let format_pod = Pod::from_bytes(&values).ok_or("failed to build audio format pod")?;
        let mut params = [format_pod];
        audio_stream
            .connect(
                Direction::Input,
                None,
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
                &mut params,
            )
            .map_err(|err| err.to_string())?;

        Some((audio_stream, audio_listener))
    } else {
        None
    };

    main_loop.run();

    // All the callbacks above hold their own `Rc` clone of `state` for as
    // long as their listener is alive; drop all of them (and the streams,
    // which own them) first so `Rc::into_inner` below actually sees a
    // unique reference.
    drop(_listener);
    drop(stream);
    drop(_audio);

    let state = Rc::into_inner(state)
        .ok_or("encoder state still referenced after main loop exited")?
        .into_inner();
    if let Some(err) = state.error {
        return Err(err);
    }
    let mut writer = state.writer;
    writer.write_end().map_err(|err| err.to_string())?;

    let duration = state.start.map(|start| start.elapsed()).unwrap_or_default();
    let file_size = std::fs::metadata(output)
        .map(|meta| meta.len())
        .unwrap_or(0);
    let (width, height) = state.size;
    let audio_note = match state.audio {
        Some(audio) => format!(", {} audio frames", audio.frame_count),
        None => String::new(),
    };
    eprintln!(
        "Wrote {} frames, {width}x{height}, {:.1}s, {:.1} KiB{audio_note} -> {}",
        state.frame_count,
        duration.as_secs_f64(),
        file_size as f64 / 1024.0,
        output.display(),
    );

    Ok(())
}

/// Builds an SPA `EnumFormat` pod offering the pixel layouts `openh264` can
/// consume directly (`BgraSliceU8`/`RgbaSliceU8`), fixed at the portal's
/// reported stream size - PipeWire negotiates down to one of these.
fn video_format_pod(size: (i32, i32)) -> Result<Vec<u8>, String> {
    let obj = object!(
        SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        property!(FormatProperties::MediaType, Id, MediaType::Video),
        property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ),
        property!(
            FormatProperties::VideoSize,
            Rectangle,
            pw::spa::utils::Rectangle {
                width: size.0.max(1) as u32,
                height: size.1.max(1) as u32,
            }
        ),
        // Without a cap, PipeWire is free to deliver frames as fast as the
        // compositor renders them - observed pegging encoding at ~100% CPU
        // continuously with no framerate offered at all. 30fps is plenty for
        // a screen recording. The minimum must include 0/1 ("variable,
        // damage-driven, no fixed rate") - screen-capture sources commonly
        // only offer that, and excluding it (an earlier version of this
        // pod had min = 1/1) made negotiation fail outright with "no more
        // input formats".
        property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: 30, denom: 1 },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction { num: 30, denom: 1 }
        ),
    );
    let values = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .map_err(|err| format!("failed to serialize format pod: {err:?}"))?
        .0
        .into_inner();
    Ok(values)
}

/// Builds an SPA `EnumFormat` pod for raw interleaved S16LE audio at
/// [`AUDIO_SAMPLE_RATE`]/[`AUDIO_CHANNELS`] - fixed values, not a `Choice`
/// range, since that's exactly what the AAC encoder is configured for.
fn audio_format_pod() -> Result<Vec<u8>, String> {
    let obj = object!(
        SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        property!(FormatProperties::MediaType, Id, MediaType::Audio),
        property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        property!(FormatProperties::AudioFormat, Id, AudioFormat::S16LE),
        property!(FormatProperties::AudioRate, Int, AUDIO_SAMPLE_RATE as i32),
        property!(FormatProperties::AudioChannels, Int, AUDIO_CHANNELS as i32),
    );
    let values = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .map_err(|err| format!("failed to serialize audio format pod: {err:?}"))?
        .0
        .into_inner();
    Ok(values)
}

/// Converts one raw frame to YUV420, encodes it, and - lazily, once the
/// first frame's SPS/PPS are known - starts the MP4 video track and writes
/// the sample.
fn encode_frame(
    state: &mut EncodeState,
    layout: PixelLayout,
    stride: usize,
    bytes: &[u8],
) -> Result<(), String> {
    let (width, height) = state.size;
    if width == 0 || height == 0 {
        return Ok(());
    }
    let packed = repack_rows(bytes, stride, width * 4, height);

    let yuv = match layout {
        PixelLayout::Bgra => {
            YUVBuffer::from_bgra8_source(BgraSliceU8::new(&packed, (width, height)))
        }
        PixelLayout::Rgba => {
            YUVBuffer::from_rgba8_source(RgbaSliceU8::new(&packed, (width, height)))
        }
    };

    let bitstream = state.encoder.encode(&yuv).map_err(|err| err.to_string())?;

    let mut sps = None;
    let mut pps = None;
    let mut sample_bytes = Vec::new();
    for l in 0..bitstream.num_layers() {
        let Some(layer) = bitstream.layer(l) else {
            continue;
        };
        for n in 0..layer.nal_count() {
            let Some(nal) = layer.nal_unit(n) else {
                continue;
            };
            let Some((nal_type, payload)) = split_annex_b_nal(nal) else {
                continue;
            };
            match nal_type {
                7 => sps = Some(payload.to_vec()),
                8 => pps = Some(payload.to_vec()),
                _ => {
                    sample_bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                    sample_bytes.extend_from_slice(payload);
                }
            }
        }
    }

    if state.video_track_id.is_none() {
        let (Some(sps), Some(pps)) = (sps, pps) else {
            // No parameter sets yet (shouldn't happen on the first frame,
            // but nothing to mux until they arrive).
            return Ok(());
        };
        let avc_config = AvcConfig {
            width: width as u16,
            height: height as u16,
            seq_param_set: sps,
            pic_param_set: pps,
        };
        state
            .writer
            .add_track(&TrackConfig::from(avc_config))
            .map_err(|err| err.to_string())?;
        state.video_track_id = Some(state.next_track_id);
        state.next_track_id += 1;
    }

    if sample_bytes.is_empty() {
        return Ok(());
    }
    let start = *state.start.get_or_insert_with(Instant::now);
    let start_time = start.elapsed().as_millis() as u64;
    let is_sync = bitstream.frame_type() == openh264::encoder::FrameType::IDR;
    let track_id = state.video_track_id.expect("set above");
    state
        .writer
        .write_sample(
            track_id,
            &Mp4Sample {
                start_time,
                duration: 0,
                rendering_offset: 0,
                is_sync,
                bytes: sample_bytes.into(),
            },
        )
        .map_err(|err| err.to_string())?;
    state.frame_count += 1;

    Ok(())
}

/// Accumulates interleaved S16LE PCM samples from one PipeWire audio
/// buffer, encoding complete AAC frames (`AAC_FRAME_SAMPLES` per channel)
/// as they become available, then muxes each one. Leftover samples
/// smaller than a full frame stay buffered for the next call.
fn encode_audio(state: &mut EncodeState, bytes: &[u8]) -> Result<(), String> {
    if state.audio.is_none() {
        return Ok(());
    }

    // Drain complete AAC frames into owned buffers first, scoping the
    // `state.audio` borrow tightly to just this block - muxing each frame
    // afterward needs sibling fields (`writer`, `next_track_id`, `start`)
    // on the same `state`, which a borrow held across this whole function
    // would conflict with.
    let mut encoded_frames = Vec::new();
    {
        let audio = state.audio.as_mut().expect("checked above");
        // SAFETY-free: reinterpret raw S16LE bytes as i16 samples by
        // pairing adjacent bytes, rather than an unaligned cast - `bytes`
        // comes from a PipeWire buffer with no alignment guarantee.
        audio.pcm.extend(
            bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| i16::from_le_bytes(*pair)),
        );

        let frame_len = AAC_FRAME_SAMPLES * AUDIO_CHANNELS as usize;
        let mut output = [0u8; 4096];
        while audio.pcm.len() >= frame_len {
            let info = audio
                .encoder
                .encode(&audio.pcm[..frame_len], &mut output)
                .map_err(|err| format!("AAC encode failed: {err}"))?;
            audio.pcm.drain(..frame_len);
            if info.output_size > 0 {
                encoded_frames.push(output[..info.output_size].to_vec());
            }
            // A zero-length output means the encoder is still filling its
            // internal look-ahead buffer before emitting a first frame -
            // nothing to write yet, just keep feeding it.
        }
    }

    for frame in encoded_frames {
        if state
            .audio
            .as_ref()
            .expect("checked above")
            .track_id
            .is_none()
        {
            let aac_config = AacConfig {
                bitrate: 128_000,
                profile: mp4::AudioObjectType::AacLowComplexity,
                freq_index: mp4::SampleFreqIndex::Freq48000,
                chan_conf: ChannelConfig::Stereo,
            };
            state
                .writer
                .add_track(&TrackConfig::from(aac_config))
                .map_err(|err| err.to_string())?;
            let track_id = state.next_track_id;
            state.next_track_id += 1;
            state.audio.as_mut().expect("checked above").track_id = Some(track_id);
        }

        let start = *state.start.get_or_insert_with(Instant::now);
        let start_time = start.elapsed().as_millis() as u64;
        let track_id = state
            .audio
            .as_ref()
            .expect("checked above")
            .track_id
            .expect("set above");
        state
            .writer
            .write_sample(
                track_id,
                &Mp4Sample {
                    start_time,
                    duration: 0,
                    rendering_offset: 0,
                    is_sync: true,
                    bytes: frame.into(),
                },
            )
            .map_err(|err| err.to_string())?;
        state.audio.as_mut().expect("checked above").frame_count += 1;
    }

    Ok(())
}

/// Strips a leading Annex-B start code (`00 00 01` or `00 00 00 01`) off
/// `nal`, returning its NAL unit type and the remaining payload.
fn split_annex_b_nal(nal: &[u8]) -> Option<(u8, &[u8])> {
    let payload = if nal.starts_with(&[0, 0, 0, 1]) {
        &nal[4..]
    } else if nal.starts_with(&[0, 0, 1]) {
        &nal[3..]
    } else {
        return None;
    };
    let nal_type = payload.first()? & 0x1F;
    Some((nal_type, payload))
}

/// Copies `height` rows of `row_bytes` bytes each out of a possibly-padded
/// `src` buffer (row pitch `stride`) into a tightly packed `Vec<u8>` -
/// `openh264`'s slice wrappers require exactly `width * height * 4` bytes
/// with no per-row padding.
fn repack_rows(src: &[u8], stride: usize, row_bytes: usize, height: usize) -> Vec<u8> {
    if stride == row_bytes {
        return src[..row_bytes * height].to_vec();
    }
    let mut out = Vec::with_capacity(row_bytes * height);
    for row in 0..height {
        let start = row * stride;
        out.extend_from_slice(&src[start..start + row_bytes]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises `encode_frame` directly with synthetic frames - bypassing
    /// PipeWire/the portal entirely - to verify the NAL parsing, SPS/PPS
    /// extraction, and MP4 muxing logic in isolation. Confirms the result
    /// by reading it back with the `mp4` crate's own reader.
    #[test]
    fn encodes_synthetic_frames_to_a_readable_mp4() {
        const WIDTH: usize = 64;
        const HEIGHT: usize = 64;
        const FRAME_COUNT: usize = 5;

        let dir = std::env::temp_dir();
        let path = dir.join(format!("prtsc-test-{}.mp4", std::process::id()));

        let file = File::create(&path).expect("create temp file");
        let mp4_config = Mp4Config {
            major_brand: str::parse("isom").unwrap(),
            minor_version: 512,
            compatible_brands: vec![str::parse("isom").unwrap(), str::parse("avc1").unwrap()],
            timescale: TIMESCALE,
        };
        let writer = Mp4Writer::write_start(file, &mp4_config).expect("write_start");

        let api = OpenH264API::from_source();
        let encoder = Encoder::with_api_config(api, EncoderConfig::new()).expect("build encoder");

        let mut state = EncodeState {
            writer,
            next_track_id: 1,
            start: None,
            error: None,
            encoder,
            layout: Some(PixelLayout::Bgra),
            size: (WIDTH, HEIGHT),
            video_track_id: None,
            last_frame_at: None,
            frame_count: 0,
            audio: None,
        };

        for frame in 0..FRAME_COUNT {
            // A shifting solid color per frame - not a realistic screen
            // capture, but enough non-trivial pixel data to exercise the
            // encoder rather than a degenerate all-zero buffer.
            let shade = (frame * 40) as u8;
            let pixels = vec![shade; WIDTH * HEIGHT * 4];
            encode_frame(&mut state, PixelLayout::Bgra, WIDTH * 4, &pixels).expect("encode_frame");
        }

        assert!(
            state.video_track_id.is_some(),
            "no track was added - SPS/PPS never seen"
        );
        state.writer.write_end().expect("write_end");

        let file = std::fs::File::open(&path).expect("reopen mp4");
        let size = file.metadata().expect("metadata").len();
        let mp4 = mp4::Mp4Reader::read_header(std::io::BufReader::new(file), size)
            .expect("mp4 file failed to parse as valid MP4");

        assert_eq!(mp4.tracks().len(), 1, "expected exactly one track");
        let track = mp4.tracks().values().next().unwrap();
        assert_eq!(track.track_type().unwrap(), mp4::TrackType::Video);
        assert!(
            mp4.sample_count(1).unwrap() >= 1,
            "expected at least one sample to have been written"
        );

        let _ = std::fs::remove_file(&path);
    }
}
