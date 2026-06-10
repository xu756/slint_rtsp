// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: MIT

pub struct VideoPlaybackThread {
    packet_sender: smol::channel::Sender<ffmpeg_next::codec::packet::packet::Packet>,
    receiver_thread: Option<std::thread::JoinHandle<()>>,
}

impl VideoPlaybackThread {
    pub fn start(
        stream: &ffmpeg_next::format::stream::Stream,
        mut video_frame_callback: Box<dyn FnMut(&ffmpeg_next::util::frame::Video) + Send>,
    ) -> Result<Self, anyhow::Error> {
        let (packet_sender, packet_receiver) =
            smol::channel::bounded::<ffmpeg_next::codec::packet::packet::Packet>(128);

        let decoder_context = ffmpeg_next::codec::Context::from_parameters(stream.parameters())?;
        let mut packet_decoder = decoder_context.decoder().video()?;

        let mut clock = StreamClock::new(stream);

        let receiver_thread =
            std::thread::Builder::new().name("video playback thread".into()).spawn(move || {
                smol::block_on(async move {
                    let mut waiting_for_key_frame = true;

                    loop {
                        let Ok(packet) = packet_receiver.recv().await else { break };

                        smol::future::yield_now().await;

                        if packet.is_corrupt() {
                            continue;
                        }

                        if waiting_for_key_frame {
                            if !packet.is_key() {
                                continue;
                            }

                            waiting_for_key_frame = false;
                            clock.reset();
                        }

                        packet_decoder.send_packet(&packet).unwrap();

                        let mut decoded_frame = ffmpeg_next::util::frame::Video::empty();

                        while packet_decoder.receive_frame(&mut decoded_frame).is_ok() {
                            if let Some(delay) =
                                clock.convert_pts_to_instant(decoded_frame.pts())
                            {
                                smol::Timer::after(delay).await;
                            }

                            video_frame_callback(&decoded_frame);
                        }
                    }
                })
            })?;

        Ok(Self { packet_sender, receiver_thread: Some(receiver_thread) })
    }

    pub async fn receive_packet(&self, packet: ffmpeg_next::codec::packet::packet::Packet) -> bool {
        match self.packet_sender.send(packet).await {
            Ok(_) => true,
            Err(smol::channel::SendError(_)) => false,
        }
    }
}

impl Drop for VideoPlaybackThread {
    fn drop(&mut self) {
        if let Some(receiver_join_handle) = self.receiver_thread.take() {
            receiver_join_handle.join().unwrap();
        }
    }
}

struct StreamClock {
    time_base_seconds: f64,
    playback_start_time: std::time::Instant,
    first_pts: Option<i64>,
}

impl StreamClock {
    fn new(stream: &ffmpeg_next::format::stream::Stream) -> Self {
        let time_base_seconds = stream.time_base();
        let time_base_seconds =
            time_base_seconds.numerator() as f64 / time_base_seconds.denominator() as f64;

        let playback_start_time = std::time::Instant::now();

        Self {
            time_base_seconds,
            playback_start_time,
            first_pts: None,
        }
    }

    fn convert_pts_to_instant(&mut self, pts: Option<i64>) -> Option<std::time::Duration> {
        pts.and_then(|pts| {
            let first_pts = *self.first_pts.get_or_insert(pts);
            let relative_pts = pts.saturating_sub(first_pts);
            let pts_since_start =
                std::time::Duration::from_secs_f64(relative_pts as f64 * self.time_base_seconds);
            self.playback_start_time.checked_add(pts_since_start)
        })
        .map(|absolute_pts| absolute_pts.saturating_duration_since(std::time::Instant::now()))
    }

    fn reset(&mut self) {
        self.playback_start_time = std::time::Instant::now();
        self.first_pts = None;
    }
}
