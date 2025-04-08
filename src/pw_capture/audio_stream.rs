use std::{
    process::Command,
    sync::{atomic::AtomicBool, Arc},
    time::SystemTime,
};

use anyhow::Result;
use log::{debug, error, info};
use pipewire::{
    self as pw,
    context::Context,
    main_loop::MainLoop,
    properties::properties,
    spa::{
        self,
        param::format::{MediaSubtype, MediaType},
        pod::Pod,
        utils::Direction,
    },
    stream::{StreamFlags, StreamState},
};
use tokio::sync::mpsc;

#[derive(Clone, Copy)]
struct UserData {
    audio_format: spa::param::audio::AudioInfoRaw,
}

impl Default for UserData {
    fn default() -> Self {
        Self {
            audio_format: Default::default(),
        }
    }
}

pub struct AudioCapture;

impl AudioCapture {
    pub fn run(
        stream_node: u32,
        process_audio_channel: mpsc::Sender<(Vec<f32>, i64)>,
        video_ready: Arc<AtomicBool>,
        audio_ready: Arc<AtomicBool>,
        use_mic: bool,
        start_time: SystemTime,
    ) -> Result<(), pw::Error> {
        let pw_loop = MainLoop::new(None)?;
        let pw_context = Context::new(&pw_loop)?;
        let audio_core = pw_context.connect(None)?;

        let _audio_core_listener = audio_core
            .add_listener_local()
            .info(|i| info!("AUDIO CORE:\n{0:#?}", i))
            .error(|e, f, g, h| error!("{0},{1},{2},{3}", e, f, g, h))
            .done(|d, _| info!("DONE: {0}", d))
            .register();

        let data = UserData::default();

        // Audio Stream
        let audio_stream = pw::stream::Stream::new(
            &audio_core,
            "auto-screen-recorder-audio",
            properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::NODE_LATENCY => "1024/48000",
            },
        )?;

        let _audio_stream_shared_data_listener = audio_stream
            .add_local_listener_with_user_data(data)
            .state_changed(move |_, _, old, new| {
                debug!("Audio Stream State Changed: {0:?} -> {1:?}", old, new);
                audio_ready.store(
                    new == StreamState::Streaming,
                    std::sync::atomic::Ordering::Release,
                );
            })
            .param_changed(|_, udata, id, param| {
                let Some(param) = param else {
                    return;
                };
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }

                let (media_type, media_subtype) =
                    match pw::spa::param::format_utils::parse_format(param) {
                        Ok(v) => v,
                        Err(_) => return,
                    };

                // only accept raw audio
                if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                    return;
                }

                udata
                    .audio_format
                    .parse(param)
                    .expect("Failed to parse audio params");

                debug!(
                    "Capturing Rate:{} channels:{}, format: {}",
                    udata.audio_format.rate(),
                    udata.audio_format.channels(),
                    udata.audio_format.format().as_raw()
                );
            })
            .process(move |stream, _| match stream.dequeue_buffer() {
                None => debug!("Out of audio buffers"),
                Some(mut buffer) => {
                    // Wait until video is streaming before we try to process
                    if !video_ready.load(std::sync::atomic::Ordering::Acquire) {
                        return;
                    }

                    let datas = buffer.datas_mut();
                    if datas.is_empty() {
                        return;
                    }

                    let time_us = if let Ok(elapsed) = start_time.elapsed() {
                        elapsed.as_micros() as i64
                    } else {
                        0
                    };

                    let data = &mut datas[0];
                    let n_samples = data.chunk().size() / (std::mem::size_of::<f32>()) as u32;

                    if let Some(samples) = data.data() {
                        let samples_f32: &[f32] = bytemuck::cast_slice(samples);
                        let audio_samples = &samples_f32[..n_samples as usize];
                        process_audio_channel
                            .blocking_send((audio_samples.to_vec(), time_us))
                            .unwrap();
                    }
                }
            })
            .register()?;

        let audio_spa_obj = pw::spa::pod::object! {
            pw::spa::utils::SpaTypes::ObjectParamFormat,
            pw::spa::param::ParamType::EnumFormat,
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::MediaType,
                Id,
                pw::spa::param::format::MediaType::Audio
                ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::MediaSubtype,
                Id,
                pw::spa::param::format::MediaSubtype::Raw
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::AudioFormat,
                Id,
                pw::spa::param::audio::AudioFormat::F32LE
            )
        };

        let audio_spa_values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &pw::spa::pod::Value::Object(audio_spa_obj),
        )
        .unwrap()
        .0
        .into_inner();

        let mut audio_params = [Pod::from_bytes(&audio_spa_values).unwrap()];

        let sink_id_to_use = if !use_mic {
            get_default_sink_node_id()
        } else {
            Some(stream_node)
        };

        debug!("Default sink id: {:?}", sink_id_to_use);
        audio_stream.connect(
            Direction::Input,
            sink_id_to_use,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
            &mut audio_params,
        )?;

        debug!("Audio Stream: {:?}", audio_stream);

        pw_loop.run();
        Ok(())
    }
}

fn get_default_sink_node_id() -> Option<u32> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(r#"pactl list sinks | awk -v sink="$(pactl info | grep 'Default Sink' | cut -d' ' -f3)" '$0 ~ "Name: " sink { found=1 } found && /object.id/ { print $NF; exit }'"#)
        .output()
        .expect("Failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout);

    let cleaned = stdout.replace('"', "");

    cleaned.trim().parse::<u32>().ok()
}
