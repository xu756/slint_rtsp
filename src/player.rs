// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: MIT

// cSpell: ignore demuxer
use std::path::PathBuf;

mod audio;
mod video;

pub struct Player {
    demuxer_thread: Option<std::thread::JoinHandle<()>>,
}

impl Player {
    pub fn start(
        path: PathBuf,
        video_frame_callback: impl FnMut(&ffmpeg_next::util::frame::Video) + Send + 'static,
        mut error_callback: impl FnMut(String) + Send + 'static,
    ) -> Result<Self, anyhow::Error> {
        let demuxer_thread =
            std::thread::Builder::new().name("demuxer thread".into()).spawn(move || {
                smol::block_on(async move {
                    let input_url = path.as_os_str().to_string_lossy();
                    let input_context_result = if input_url.starts_with("rtsp://") {
                        let mut options = ffmpeg_next::Dictionary::new();
                        options.set("rtsp_transport", "tcp");

                        ffmpeg_next::format::input_with_dictionary(&path, options)
                    } else {
                        ffmpeg_next::format::input(&path)
                    };

                    let mut input_context = match input_context_result {
                        Ok(input_context) => input_context,
                        Err(error) => {
                            let msg = format!("failed to open input {input_url}: {error}");
                            eprintln!("{msg}");
                            error_callback(msg);
                            return;
                        }
                    };

                    let Some(video_stream) =
                        input_context.streams().best(ffmpeg_next::media::Type::Video)
                    else {
                        let msg = format!("input {input_url} has no video stream");
                        eprintln!("{msg}");
                        error_callback(msg);
                        return;
                    };
                    let video_stream_index = video_stream.index();
                    let video_playback_thread = video::VideoPlaybackThread::start(
                        &video_stream,
                        Box::new(video_frame_callback),
                    )
                    .unwrap();

                    let audio_playback_thread = input_context
                        .streams()
                        .best(ffmpeg_next::media::Type::Audio)
                        .and_then(|audio_stream| {
                            let audio_stream_index = audio_stream.index();

                            match audio::AudioPlaybackThread::start(&audio_stream) {
                                Ok(audio_playback_thread) => {
                                    Some((audio_stream_index, audio_playback_thread))
                                }
                                Err(error) => {
                                    eprintln!("failed to start audio playback: {error:#}");
                                    None
                                }
                            }
                        });

                    for (stream, packet) in input_context.packets() {
                        if let Some((audio_stream_index, audio_playback_thread)) =
                            audio_playback_thread.as_ref()
                        {
                            if stream.index() == *audio_stream_index {
                                audio_playback_thread.receive_packet(packet).await;
                                continue;
                            }
                        }

                        if stream.index() == video_stream_index {
                            video_playback_thread.receive_packet(packet).await;
                        }
                    }
                })
            })?;

        Ok(Self {
            demuxer_thread: Some(demuxer_thread),
        })
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Some(decoder_thread) = self.demuxer_thread.take() {
            decoder_thread.join().unwrap();
        }
    }
}
