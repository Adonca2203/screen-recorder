mod application_config;
mod dbus;
mod encoders;
mod pw_capture;

use std::{
    sync::{atomic::AtomicBool, Arc},
    time::SystemTime,
};

use anyhow::{Context, Error, Result};
use application_config::load_or_create_config;
use encoders::{
    audio_encoder::AudioEncoder,
    buffer::{AudioBuffer, VideoBuffer},
    video_encoder::VideoEncoder,
};
use ffmpeg_next::{self as ffmpeg};
use log::{debug, LevelFilter};
use pipewire::{self as pw};
use portal_screencast::{CursorMode, ScreenCast, SourceType};
use pw_capture::{audio_stream::AudioCapture, video_stream::VideoCapture};
use tokio::sync::{mpsc, Mutex};
use zbus::connection;

const VIDEO_STREAM: usize = 0;
const AUDIO_STREAM: usize = 1;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let _ = simple_logging::log_to_file("logs.txt", LevelFilter::Debug);
    let config = load_or_create_config();

    let mut screen_cast = ScreenCast::new()?;
    screen_cast.set_source_types(SourceType::MONITOR);
    screen_cast.set_cursor_mode(CursorMode::EMBEDDED);
    let screen_cast = screen_cast.start(None)?;

    let fd = screen_cast.pipewire_fd();
    let stream = screen_cast.streams().next().unwrap();
    let stream_node = stream.pipewire_node();
    let (width, height) = stream.size();

    let (save_tx, mut save_rx) = mpsc::channel(1);
    let clip_service = dbus::ClipService::new(save_tx);

    debug!("Creating dbus connection");
    let _connection = connection::Builder::session()?
        .name("com.rust.GameClip")?
        .serve_at("/com/rust/GameClip", clip_service)?
        .build()
        .await?;

    let (video_sender, mut video_receiver) = mpsc::channel::<(Vec<u8>, i64)>(10);
    let (audio_sender, mut audio_receiver) = mpsc::channel::<(Vec<f32>, i64)>(10);

    let video_encoder = Arc::new(Mutex::new(VideoEncoder::new(
        width,
        height,
        config.max_seconds,
        &config.encoder,
    )?));
    let audio_encoder = Arc::new(Mutex::new(AudioEncoder::new(config.max_seconds)?));

    let video_ready = Arc::new(AtomicBool::new(false));
    let audio_ready = Arc::new(AtomicBool::new(false));

    let vr_clone = Arc::clone(&video_ready);
    let ar_clone = Arc::clone(&audio_ready);
    pw::init();

    let current_time = SystemTime::now();

    std::thread::spawn(move || {
        debug!("Starting video stream");
        let _video = VideoCapture::run(
            fd,
            stream_node,
            video_sender,
            video_ready,
            audio_ready,
            current_time,
        )
        .unwrap();
    });

    std::thread::spawn(move || {
        debug!("Starting audio stream");
        let _audio = AudioCapture::run(
            stream_node,
            audio_sender,
            vr_clone,
            ar_clone,
            config.use_mic,
            current_time,
        )
        .unwrap();
    });

    // Main event loop
    loop {
        tokio::select! {
            _ = save_rx.recv() => {
                // Stop capturing video and audio while we save by taking out the locks
                let (mut video_lock, mut audio_lock) = tokio::join!(
                    video_encoder.lock(),
                    audio_encoder.lock()
                );

                // Drain both encoders of any remaining frames being processed
                video_lock.drain()?;
                audio_lock.drain()?;

                let filename = format!("clip_{}.mp4", chrono::Local::now().timestamp());
                let video_buffer = video_lock.get_buffer();
                let video_encoder = video_lock
                    .get_encoder()
                    .as_ref()
                    .context("Could not get video encoder")?;

                let audio_buffer = audio_lock.get_buffer();
                let audio_encoder = audio_lock
                    .get_encoder()
                    .as_ref()
                    .context("Could not get audio encoder")?;

                save_buffer(&filename, video_buffer, video_encoder, audio_buffer, audio_encoder)?;

                video_lock.reset_encoder()?;
                audio_lock.reset_encoder()?;

                debug!("Done saving!");
            },
            Some((frame, time)) = video_receiver.recv() => {
                video_encoder.lock().await.process(&frame, time)?;
            },
            Some((samples, time)) = audio_receiver.recv() => {
                audio_encoder.lock().await.process(&samples, time)?;
            }
        }
    }
}

fn save_buffer(
    filename: &str,
    video_buffer: &VideoBuffer,
    video_encoder: &ffmpeg::codec::encoder::Video,
    audio_buffer: &AudioBuffer,
    audio_encoder: &ffmpeg::codec::encoder::Audio,
) -> Result<()> {
    let mut output = ffmpeg::format::output(&filename)?;

    let video_codec = video_encoder
        .codec()
        .context("Could not find expected video codec")?;

    let mut video_stream = output.add_stream(video_codec)?;
    video_stream.set_time_base(video_encoder.time_base());
    video_stream.set_parameters(&video_encoder);

    let audio_codec = audio_encoder
        .codec()
        .context("Could not find expected audio codec")?;

    let mut audio_stream = output.add_stream(audio_codec)?;
    audio_stream.set_time_base(audio_encoder.time_base());
    audio_stream.set_parameters(&audio_encoder);

    output.write_header()?;

    let last_keyframe = video_buffer
        .get_last_gop_start()
        .context("Could not get last keyframe dts")?;

    let newest_video_pts = video_buffer
        .get_frames()
        .get(last_keyframe)
        .context("Could not get last keyframe")?
        .get_pts();

    // Write video
    let first_pts_offset = video_buffer
        .oldest_pts()
        .context("Could not get oldest pts when muxing.")?;
    debug!("VIDEO SAVE START");
    for (dts, frame_data) in video_buffer.get_frames().range(..=last_keyframe) {
        let pts_offset = frame_data.get_pts() - first_pts_offset;
        let mut dts_offset = dts - first_pts_offset;

        debug!("PTS offset: {:?}", pts_offset);
        if dts_offset < 0 {
            dts_offset = 0;
        }

        let mut packet = ffmpeg::codec::packet::Packet::copy(&frame_data.get_raw_bytes());
        packet.set_pts(Some(pts_offset));
        packet.set_dts(Some(dts_offset));

        packet.set_stream(VIDEO_STREAM);

        packet
            .write_interleaved(&mut output)
            .expect("Could not write video interleaved");
    }
    debug!("VIDEO SAVE END");

    // Write audio
    let oldest_frame_offset = audio_buffer
        .oldest_pts()
        .context("Could not get oldest chunk")?;

    debug!("AUDIO SAVE START");
    for (pts_in_micros, frame) in audio_buffer.get_frames() {
        // Don't write any more audio if we would exceed video (clip to max video)
        if pts_in_micros > newest_video_pts {
            break;
        }

        let offset = frame.get_pts() - oldest_frame_offset;

        debug!(
            "PTS IN MICROS: {:?}, PTS IN TIME SCALE: {:?}",
            pts_in_micros, offset
        );

        let mut packet = ffmpeg::codec::packet::Packet::copy(&frame.get_data());
        packet.set_pts(Some(offset));
        packet.set_dts(Some(offset));

        packet.set_stream(AUDIO_STREAM);

        packet
            .write_interleaved(&mut output)
            .expect("Could not write audio interleaved");
    }
    debug!("AUDIO SAVE END");

    output.write_trailer()?;

    Ok(())
}
