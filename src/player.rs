// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: MIT

// cSpell: ignore demuxer
use std::path::PathBuf;

use futures::{FutureExt, future::OptionFuture};

mod audio;
mod video;

#[derive(Clone, Copy)]
pub enum ControlCommand {
    Play,
    Pause,
}

pub struct Player {
    control_sender: smol::channel::Sender<ControlCommand>,
    demuxer_thread: Option<std::thread::JoinHandle<()>>,
    playing: bool,
    playing_changed_callback: Box<dyn Fn(bool)>,
}

impl Player {
    pub fn start(
        path: PathBuf,
        video_frame_callback: impl FnMut(&ffmpeg_next::util::frame::Video) + Send + 'static,
        playing_changed_callback: impl Fn(bool) + 'static,
    ) -> Result<Self, anyhow::Error> {
        let (control_sender, control_receiver) = smol::channel::unbounded();

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
                            eprintln!("failed to open input {input_url}: {error}");
                            return;
                        }
                    };

                    let Some(video_stream) =
                        input_context.streams().best(ffmpeg_next::media::Type::Video)
                    else {
                        eprintln!("input {input_url} has no video stream");
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

                    let mut playing = true;

                    // This is sub-optimal, as reading the packets from ffmpeg might be blocking
                    // and the future won't yield for that. So while ffmpeg sits on some blocking
                    // I/O operation, the caller here will also block and we won't end up polling
                    // the control_receiver future further down.
                    let packet_forwarder_impl = async {
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
                    }
                    .fuse()
                    .shared();

                    loop {
                        // This is sub-optimal, as reading the packets from ffmpeg might be blocking
                        // and the future won't yield for that. So while ffmpeg sits on some blocking
                        // I/O operation, the caller here will also block and we won't end up polling
                        // the control_receiver future further down.
                        let packet_forwarder: OptionFuture<_> =
                            if playing { Some(packet_forwarder_impl.clone()) } else { None }.into();

                        smol::pin!(packet_forwarder);

                        futures::select! {
                            _ = packet_forwarder => {}, // playback finished
                            received_command = control_receiver.recv().fuse() => {
                                match received_command {
                                    Ok(command) => {
                                        video_playback_thread.send_control_message(command).await;
                                        if let Some((_, audio_playback_thread)) =
                                            audio_playback_thread.as_ref()
                                        {
                                            audio_playback_thread.send_control_message(command).await;
                                        }
                                        match command {
                                            ControlCommand::Play => {
                                                // Continue in the loop, polling the packet forwarder future to forward
                                                // packets
                                                playing = true;
                                            },
                                            ControlCommand::Pause => {
                                                playing = false;
                                            }
                                        }
                                    }
                                    Err(_) => {
                                        // Channel closed -> quit
                                        return;
                                    }
                                }
                            }
                        }
                    }
                })
            })?;

        let playing = true;
        playing_changed_callback(playing);

        Ok(Self {
            control_sender,
            demuxer_thread: Some(demuxer_thread),
            playing,
            playing_changed_callback: Box::new(playing_changed_callback),
        })
    }

    pub fn toggle_pause_playing(&mut self) {
        if self.playing {
            self.playing = false;
            self.control_sender
                .send_blocking(ControlCommand::Pause)
                .unwrap();
        } else {
            self.playing = true;
            self.control_sender
                .send_blocking(ControlCommand::Play)
                .unwrap();
        }
        (self.playing_changed_callback)(self.playing);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.control_sender.close();
        if let Some(decoder_thread) = self.demuxer_thread.take() {
            decoder_thread.join().unwrap();
        }
    }
}
