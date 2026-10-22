//! HEVC decoding on a worker thread via libavcodec. The wire's hvcC/length-prefixed
//! samples become Annex B here (`crate::hevc`), without changing the protocol.
//!
//! Output is packed NV12 (`stride == width`), the same layout the Windows decoder
//! hands its renderer, so the GPU does the YUV→RGB conversion and nothing in this
//! module ever scales (invariants I-1): a frame whose size differs from the
//! host's display is an error, not something to resample.
use crate::hevc::HevcConfig;
use ffmpeg_next as ffmpeg;
use ffmpeg::codec::{self, threading};
use ffmpeg::format::Pixel;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

// Three access units is enough to absorb a short decoder hiccup without turning
// the queue into visible remote-desktop latency. If we overflow, we discard the
// dependency chain and ask the host for a fresh keyframe.
const MAX_PENDING_FRAMES: usize = 3;
const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;

/// A decoded frame: the Y plane followed by the interleaved UV plane, rows packed.
#[derive(Debug)]
pub struct DecodedFrame {
    pub nv12: Vec<u8>,
    pub stride: usize,
}

#[derive(Default)]
struct PendingInput {
    frames: VecDeque<EncodedFrame>,
    bytes: usize,
    awaiting_keyframe: bool,
    stopped: bool,
}
struct EncodedFrame {
    data: Vec<u8>,
    keyframe: bool,
    pts_micros: u64,
    reset: bool,
}
impl PendingInput {
    /// Queue one compressed access unit. Returns true when the dependency chain
    /// was discarded and the caller should request a new keyframe.
    fn push(&mut self, mut frame: EncodedFrame) -> bool {
        if self.stopped {
            return false;
        }
        let mut request_keyframe = false;
        if self.frames.len() >= MAX_PENDING_FRAMES
            || self.bytes + frame.data.len() > MAX_PENDING_BYTES
        {
            self.frames.clear();
            self.bytes = 0;
            self.awaiting_keyframe = true;
            request_keyframe = true;
        }
        if frame.data.len() > MAX_PENDING_BYTES {
            self.awaiting_keyframe = true;
            return request_keyframe;
        }
        if self.awaiting_keyframe {
            if !frame.keyframe {
                return request_keyframe;
            }
            frame.reset = true;
            self.awaiting_keyframe = false;
        }
        self.bytes += frame.data.len();
        self.frames.push_back(frame);
        request_keyframe
    }
    fn pop(&mut self) -> Option<EncodedFrame> {
        let frame = self.frames.pop_front()?;
        self.bytes -= frame.data.len();
        Some(frame)
    }
}

/// Preserve compressed dependencies in a bounded queue. Only decoded output
/// may safely use a latest-frame mailbox.
pub struct DecoderWorker {
    input: Arc<(Mutex<PendingInput>, Condvar)>,
    output: Arc<Mutex<Option<DecodedFrame>>>,
    error: Arc<Mutex<Option<String>>>,
    keyframe_request: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl DecoderWorker {
    pub fn start(hvcc: Vec<u8>, width: u32, height: u32) -> std::io::Result<Self> {
        let input = Arc::new((
            Mutex::new(PendingInput {
                awaiting_keyframe: true,
                ..Default::default()
            }),
            Condvar::new(),
        ));
        let output = Arc::new(Mutex::new(None));
        let error = Arc::new(Mutex::new(None));
        let worker_input = Arc::clone(&input);
        let worker_output = Arc::clone(&output);
        let worker_error = Arc::clone(&error);
        let keyframe_request = Arc::new(AtomicBool::new(false));
        let worker_keyframe_request = Arc::clone(&keyframe_request);
        let thread = thread::Builder::new()
            .name("transom-decode".into())
            .spawn(move || {
                let result = (|| -> Result<(), String> {
                    let mut decoder = Decoder::new(&hvcc, width, height)?;
                    let mut recover = false;
                    let mut decoded = 0;
                    loop {
                        let encoded = {
                            let (lock, ready) = &*worker_input;
                            let mut pending = lock.lock().unwrap_or_else(|e| e.into_inner());
                            while pending.frames.is_empty() && !pending.stopped {
                                pending = ready.wait(pending).unwrap_or_else(|e| e.into_inner());
                            }
                            if pending.stopped {
                                break;
                            }
                            pending.pop().expect("queue checked above")
                        };
                        if recover && !encoded.keyframe {
                            continue;
                        }
                        if encoded.reset || recover {
                            eprintln!(
                                "video: resuming decode at keyframe ({} bytes)",
                                encoded.data.len()
                            );
                            decoder.flush();
                            recover = false;
                        }
                        match decoder.decode(&encoded.data, encoded.keyframe, encoded.pts_micros) {
                            Ok(Some(frame)) => {
                                decoded += 1;
                                if decoded == 1 {
                                    eprintln!(
                                        "video: first decoded NV12 frame ({} bytes)",
                                        frame.nv12.len()
                                    );
                                }
                                *worker_output.lock().unwrap_or_else(|e| e.into_inner()) =
                                    Some(frame)
                            }
                            Ok(None) => {}
                            Err(e) => {
                                eprintln!("video: {e}");
                                worker_keyframe_request.store(true, Ordering::Release);
                                *worker_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
                                recover = true;
                            }
                        }
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    eprintln!("video: {e}");
                    *worker_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
                }
                let mut pending = worker_input.0.lock().unwrap_or_else(|e| e.into_inner());
                pending.stopped = true;
                pending.frames.clear();
                pending.bytes = 0;
            })?;
        Ok(Self {
            input,
            output,
            error,
            keyframe_request,
            thread: Some(thread),
        })
    }
    pub fn submit(&self, data: Vec<u8>, keyframe: bool, pts_micros: u64) {
        let (lock, ready) = &*self.input;
        let dropped_chain = lock
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(EncodedFrame {
                data,
                keyframe,
                pts_micros,
                reset: false,
            });
        if dropped_chain {
            self.keyframe_request.store(true, Ordering::Release);
        }
        ready.notify_one();
    }
    pub fn take_frame(&self) -> Option<DecodedFrame> {
        self.output.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
    pub fn take_error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
    pub fn take_keyframe_request(&self) -> bool {
        self.keyframe_request.swap(false, Ordering::AcqRel)
    }
}
impl Drop for DecoderWorker {
    fn drop(&mut self) {
        let (lock, ready) = &*self.input;
        {
            let mut pending = lock.lock().unwrap_or_else(|e| e.into_inner());
            pending.stopped = true;
            pending.frames.clear();
            ready.notify_one();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Decoder {
    decoder: ffmpeg::decoder::Video,
    config: HevcConfig,
    width: u32,
    height: u32,
    scratch: ffmpeg::frame::Video,
}

/// Frame-threaded decoding adds up to `threads - 1` frames of latency; four
/// keeps 4K60 software decode comfortable while staying near 50 ms worst case.
/// `TRANSOM_DECODE_THREADS=1` trades throughput for the lowest latency.
fn decode_threads() -> usize {
    std::env::var("TRANSOM_DECODE_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| (1..=32).contains(n))
        .unwrap_or_else(|| {
            thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .min(4)
        })
}

impl Decoder {
    fn new(hvcc: &[u8], width: u32, height: u32) -> Result<Self, String> {
        let config = HevcConfig::parse(hvcc)?;
        if config.chroma != 1 || config.bit_depth != 8 {
            return Err(format!("Host sent HEVC chroma {} / {}-bit. Select 4:2:0 8-bit in Mac Video settings and restart sharing.", config.chroma, config.bit_depth));
        }
        ffmpeg::init().map_err(|e| format!("FFmpeg initialization: {e}"))?;
        let codec = ffmpeg::decoder::find(codec::Id::HEVC).ok_or(
            "FFmpeg has no HEVC decoder. Fedora's ffmpeg-free omits it: install libavcodec-freeworld from RPM Fusion (or any FFmpeg build with HEVC)."
                .to_string(),
        )?;
        let mut context = codec::context::Context::new_with_codec(codec);
        context.set_flags(codec::Flags::LOW_DELAY);
        let threads = decode_threads();
        context.set_threading(threading::Config {
            kind: if threads > 1 {
                threading::Type::Frame
            } else {
                threading::Type::None
            },
            count: threads,
            ..Default::default()
        });
        let decoder = context
            .decoder()
            .video()
            .map_err(|e| format!("Open HEVC decoder: {e}"))?;
        eprintln!(
            "video: HEVC decoder ready, {width}x{height}, Main 4:2:0 8-bit, Annex B, {threads} thread(s)"
        );
        Ok(Self {
            decoder,
            config,
            width,
            height,
            scratch: ffmpeg::frame::Video::empty(),
        })
    }

    fn flush(&mut self) {
        self.decoder.flush();
    }

    fn decode(
        &mut self,
        au: &[u8],
        keyframe: bool,
        pts_micros: u64,
    ) -> Result<Option<DecodedFrame>, String> {
        let bytes = self.config.annex_b(au, keyframe)?;
        let mut packet = ffmpeg::Packet::copy(&bytes);
        packet.set_pts(Some(pts_micros.min(i64::MAX as u64) as i64));
        if keyframe {
            packet.set_flags(codec::packet::Flags::KEY);
        }
        let mut latest = None;
        match self.decoder.send_packet(&packet) {
            Ok(()) => {}
            Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => {
                // The decoder has output pending. Drain it, then retry the same
                // packet so compressed dependencies are preserved.
                latest = self.drain()?;
                self.decoder
                    .send_packet(&packet)
                    .map_err(|e| format!("Retry HEVC input: {e}"))?;
            }
            Err(e) => return Err(format!("Decode HEVC input: {e}")),
        }
        Ok(self.drain()?.or(latest))
    }

    /// Pull every frame the decoder has ready, keeping only the newest.
    fn drain(&mut self) -> Result<Option<DecodedFrame>, String> {
        let mut latest = None;
        loop {
            match self.decoder.receive_frame(&mut self.scratch) {
                Ok(()) => latest = Some(to_nv12(&self.scratch, self.width, self.height)?),
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => {
                    return Ok(latest)
                }
                Err(ffmpeg::Error::Eof) => return Ok(latest),
                Err(e) => return Err(format!("Decode HEVC output: {e}")),
            }
        }
    }
}

/// Repack a decoded frame as tightly packed NV12. Refuses any size change: the
/// renderer samples 1:1 and must never be handed a frame it would have to scale.
fn to_nv12(frame: &ffmpeg::frame::Video, width: u32, height: u32) -> Result<DecodedFrame, String> {
    if frame.width() != width || frame.height() != height {
        return Err(format!(
            "Decoded size {}x{} differs from host display {width}x{height}; refusing to scale",
            frame.width(),
            frame.height()
        ));
    }
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
        return Err("Invalid NV12 output dimensions".into());
    }
    let mut nv12 = vec![0u8; w * h * 3 / 2];
    let (luma, chroma) = nv12.split_at_mut(w * h);
    let y_stride = frame.stride(0);
    let y_src = frame.data(0);
    for row in 0..h {
        luma[row * w..(row + 1) * w].copy_from_slice(&y_src[row * y_stride..row * y_stride + w]);
    }
    match frame.format() {
        Pixel::YUV420P | Pixel::YUVJ420P => {
            let (u_src, v_src) = (frame.data(1), frame.data(2));
            let (u_stride, v_stride) = (frame.stride(1), frame.stride(2));
            for row in 0..h / 2 {
                let out = &mut chroma[row * w..(row + 1) * w];
                let u = &u_src[row * u_stride..row * u_stride + w / 2];
                let v = &v_src[row * v_stride..row * v_stride + w / 2];
                for (i, pair) in out.chunks_exact_mut(2).enumerate() {
                    pair[0] = u[i];
                    pair[1] = v[i];
                }
            }
        }
        Pixel::NV12 => {
            let (uv_src, uv_stride) = (frame.data(1), frame.stride(1));
            for row in 0..h / 2 {
                chroma[row * w..(row + 1) * w]
                    .copy_from_slice(&uv_src[row * uv_stride..row * uv_stride + w]);
            }
        }
        other => return Err(format!("Unsupported decoder pixel format {other:?}")),
    }
    Ok(DecodedFrame { nv12, stride: w })
}

/// Report whether libavcodec can decode HEVC, for `doctor`.
pub fn probe() -> Result<String, String> {
    ffmpeg::init().map_err(|e| format!("FFmpeg initialization: {e}"))?;
    let codec = ffmpeg::decoder::find(codec::Id::HEVC)
        .ok_or("FFmpeg has no HEVC decoder (Fedora's ffmpeg-free omits it: install libavcodec-freeworld from RPM Fusion)")?;
    Ok(format!(
        "{} ({}), {} thread(s)",
        codec.name(),
        codec.description(),
        decode_threads()
    ))
}

/// BT.709 limited-range NV12 → BGRA on the CPU. The renderer does this on the GPU;
/// this copy exists so tests can check decoded pixels.
#[cfg(test)]
fn nv12_to_bgra(nv12: &[u8], width: u32, height: u32, stride: usize) -> Option<Vec<u8>> {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 || stride < w {
        return None;
    }
    let y_size = stride.checked_mul(h)?;
    let needed = y_size.checked_add(stride.checked_mul(h / 2)?)?;
    if nv12.len() < needed {
        return None;
    }
    let (y_plane, uv_plane) = nv12.split_at(y_size);
    let mut bgra = vec![0; w.checked_mul(h)?.checked_mul(4)?];
    for row in 0..h {
        for col in 0..w {
            let y = i32::from(y_plane[row * stride + col]) - 16;
            let uv = (row / 2) * stride + (col / 2) * 2;
            let u = i32::from(uv_plane[uv]) - 128;
            let v = i32::from(uv_plane[uv + 1]) - 128;
            let o = (row * w + col) * 4;
            bgra[o] = ((298 * y + 541 * u + 128) >> 8).clamp(0, 255) as u8;
            bgra[o + 1] = ((298 * y - 55 * u - 136 * v + 128) >> 8).clamp(0, 255) as u8;
            bgra[o + 2] = ((298 * y + 459 * v + 128) >> 8).clamp(0, 255) as u8;
            bgra[o + 3] = 255;
        }
    }
    Some(bgra)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_reports_invalid_config_without_waiting_for_video_frames() {
        let worker = DecoderWorker::start(vec![1, 2], 128, 96).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some(error) = worker.take_error() {
                assert!(error.contains("HEVC configuration"), "{error}");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worker failed silently"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn encoded(n: u64, keyframe: bool) -> EncodedFrame {
        EncodedFrame {
            data: vec![n as u8],
            keyframe,
            pts_micros: n,
            reset: false,
        }
    }
    #[test]
    fn compressed_frames_keep_order_and_recover_at_keyframe_after_overflow() {
        let mut queue = PendingInput {
            awaiting_keyframe: true,
            ..Default::default()
        };
        queue.push(encoded(0, false));
        assert!(queue.pop().is_none());
        queue.push(encoded(1, true));
        queue.push(encoded(2, false));
        assert!(queue.pop().unwrap().reset);
        assert_eq!(queue.pop().unwrap().pts_micros, 2);
        for n in 0..=MAX_PENDING_FRAMES {
            queue.push(encoded(n as u64, false));
        }
        assert!(queue.pop().is_none());
        queue.push(encoded(10, false));
        assert!(queue.pop().is_none());
        queue.push(encoded(11, true));
        assert!(queue.pop().unwrap().reset);
    }

    /// Decodes the same real HEVC fixture the Windows decoder test uses. Ignored in
    /// ordinary runs because HEVC decoding is not in every FFmpeg build (Fedora's
    /// ffmpeg-free omits it); run with `cargo test decodes_hevc_fixture -- --ignored`.
    #[test]
    #[ignore = "Requires an FFmpeg build with an HEVC decoder"]
    fn decodes_hevc_fixture() {
        use crate::net::FramedReceiver;
        use crate::wire::VideoMessage;
        let data = include_bytes!("../../tests/fixtures/hevc-main.wire");
        let mut wire = FramedReceiver::new(&data[..]);
        let VideoMessage::Config { hvcc } =
            VideoMessage::decode(&wire.recv().unwrap().unwrap()).unwrap()
        else {
            panic!("config expected")
        };
        let mut decoder = Decoder::new(&hvcc, 128, 96).unwrap();
        let mut count = 0;
        while let Some(payload) = wire.recv().unwrap() {
            let VideoMessage::Frame {
                data,
                keyframe,
                pts_micros,
                ..
            } = VideoMessage::decode(&payload).unwrap()
            else {
                panic!("frame expected")
            };
            if let Some(frame) = decoder.decode(&data, keyframe, pts_micros).unwrap() {
                assert_eq!(frame.stride, 128);
                assert_eq!(frame.nv12.len(), 128 * 96 * 3 / 2);
                let bgra = nv12_to_bgra(&frame.nv12, 128, 96, frame.stride).unwrap();
                assert!(
                    bgra.chunks_exact(4).any(|p| p[0].abs_diff(p[2]) > 100),
                    "test colors must survive decode"
                );
                count += 1;
            }
        }
        eprintln!("libavcodec decoded {count} real HEVC frames at 128x96");
        assert!(count >= 8, "expected most frames, got {count}");
    }

    #[test]
    fn rejects_a_size_that_would_need_scaling() {
        let mut frame = ffmpeg::frame::Video::new(Pixel::YUV420P, 64, 48);
        for plane in 0..3 {
            frame.data_mut(plane).fill(0);
        }
        assert!(to_nv12(&frame, 128, 96).is_err());
        let ok = to_nv12(&frame, 64, 48).unwrap();
        assert_eq!(ok.nv12.len(), 64 * 48 * 3 / 2);
    }
}
