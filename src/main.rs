slint::include_modules!();

use ffmpeg_next::format::Pixel;
use slint::Model;

mod player;

const VIDEO_URL: &str = "rtmp://127.0.0.1:1935/live/predict";
const LOG_SNAPSHOT_URL: &str = "https://picsum.photos/seed/rtsp-log/720/405";
const ALARM_SNAPSHOT_URL: &str = "https://picsum.photos/seed/rtsp-alarm/720/405";

fn filter_stderr() {
    use std::io::Write;
    use std::os::unix::io::FromRawFd;

    unsafe {
        let original_stderr = libc::dup(libc::STDERR_FILENO);
        let mut pipe_fds = [0; 2];
        libc::pipe(pipe_fds.as_mut_ptr());
        libc::dup2(pipe_fds[1], libc::STDERR_FILENO);
        libc::close(pipe_fds[1]);

        std::thread::spawn(move || {
            let pipe_read = std::fs::File::from_raw_fd(pipe_fds[0]);
            let mut original_stderr_file = std::fs::File::from_raw_fd(original_stderr);
            use std::io::BufRead;
            let reader = std::io::BufReader::new(pipe_read);
            for line in reader.lines() {
                if let Ok(l) = line {
                    if !l.contains("ICU4X data error") {
                        let _ = writeln!(original_stderr_file, "{}", l);
                    }
                }
            }
        });
    }
}

fn main() -> anyhow::Result<()> {
    filter_stderr();
    ffmpeg_next::init()?;

    let app = App::new()?;

    let url = std::path::PathBuf::from(VIDEO_URL);
    let mut to_rgb_rescaler: Option<Rescaler> = None;
    let log_snapshot = load_http_image(LOG_SNAPSHOT_URL).unwrap_or_default();
    let alarm_snapshot = load_http_image(ALARM_SNAPSHOT_URL).unwrap_or_default();

    let logs_model = std::rc::Rc::new(slint::VecModel::default());
    logs_model.push(LogMessage {
        time: "2026-06-10 20:30:00".into(),
        level: "INFO".into(),
        message: "Edge node initialized successfully.".into(),
        detail: "边缘节点启动完成，视频拉流、解码线程和系统指标采集均已进入工作状态。".into(),
        image_url: LOG_SNAPSHOT_URL.into(),
        snapshot: log_snapshot.clone(),
    });
    logs_model.push(LogMessage {
        time: "2026-06-10 20:35:12".into(),
        level: "WARN".into(),
        message: "High latency detected on stream.".into(),
        detail:
            "检测到当前视频流链路延迟升高，建议检查网络抖动、推流端负载和 RTSP/RTMP 服务端状态。"
                .into(),
        image_url: LOG_SNAPSHOT_URL.into(),
        snapshot: log_snapshot.clone(),
    });
    app.set_logs(logs_model.clone().into());
    app.set_log_count(logs_model.row_count() as i32);

    let alarms_model = std::rc::Rc::new(slint::VecModel::default());
    alarms_model.push(AlarmMessage {
        time: "16:48:25".into(),
        level: "高危".into(),
        message: "人员闯入检测".into(),
        status: "未处理".into(),
        detail: "检测区域 A-03 出现人员闯入，目标停留超过阈值。请核验现场画面并尽快处理。".into(),
        image_url: ALARM_SNAPSHOT_URL.into(),
        snapshot: alarm_snapshot.clone(),
    });
    alarms_model.push(AlarmMessage {
        time: "16:47:30".into(),
        level: "中危".into(),
        message: "区域入侵检测".into(),
        status: "未处理".into(),
        detail: "周界区域出现移动目标，系统已截取触发帧用于复核。".into(),
        image_url: ALARM_SNAPSHOT_URL.into(),
        snapshot: alarm_snapshot.clone(),
    });
    alarms_model.push(AlarmMessage {
        time: "16:45:12".into(),
        level: "低危".into(),
        message: "移动物体检测".into(),
        status: "已处理".into(),
        detail: "画面左侧出现短时移动目标，已完成人工确认并归档。".into(),
        image_url: ALARM_SNAPSHOT_URL.into(),
        snapshot: alarm_snapshot.clone(),
    });
    app.set_alarms(alarms_model.clone().into());
    app.set_alarm_count(alarms_model.row_count() as i32);

    let app_weak_for_error = app.as_weak();

    let sysinfo_timer = slint::Timer::default();
    let sysinfo_app_weak = app.as_weak();
    let system = std::rc::Rc::new(std::cell::RefCell::new(sysinfo::System::new_all()));
    let networks = std::rc::Rc::new(std::cell::RefCell::new(
        sysinfo::Networks::new_with_refreshed_list(),
    ));

    sysinfo_timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_secs(1),
        move || {
            if let Some(app) = sysinfo_app_weak.upgrade() {
                let mut sys = system.borrow_mut();
                sys.refresh_cpu_usage();
                sys.refresh_memory();

                let mut nets = networks.borrow_mut();
                nets.refresh(true);

                let cpu_usage = sys.global_cpu_usage();
                app.set_cpu_usage(format!("{:.1}%", cpu_usage).into());

                let mem_used_mb = sys.used_memory() / 1024 / 1024;
                app.set_mem_usage(format!("{}MB", mem_used_mb).into());

                let mut total_rx: u64 = 0;
                let mut total_tx: u64 = 0;
                for (_name, data) in nets.iter() {
                    total_rx += data.received();
                    total_tx += data.transmitted();
                }
                let total_bps = (total_rx + total_tx) * 8;
                let total_mbps = total_bps as f64 / 1_000_000.0;
                app.set_net_usage(format!("{:.1}Mbps", total_mbps).into());

                let time_str = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
                app.set_system_time(time_str.into());
            }
        },
    );

    let _player = player::Player::start(
        url.into(),
        {
            let app_weak = app.as_weak();

            move |new_frame| {
                let rebuild_rescaler = to_rgb_rescaler.as_ref().is_none_or(|existing_rescaler| {
                    existing_rescaler.input().format != new_frame.format()
                        || existing_rescaler.input().width != new_frame.width()
                        || existing_rescaler.input().height != new_frame.height()
                });

                if rebuild_rescaler {
                    to_rgb_rescaler = Some(rgb_rescaler_for_frame(new_frame));
                }

                let rescaler = to_rgb_rescaler.as_mut().unwrap();

                let mut rgb_frame = ffmpeg_next::util::frame::Video::empty();
                rescaler.run(new_frame, &mut rgb_frame).unwrap();

                let pixel_buffer = video_frame_to_pixel_buffer(&rgb_frame);

                let width = new_frame.width();
                let height = new_frame.height();
                app_weak
                    .upgrade_in_event_loop(move |app| {
                        app.set_video_frame(slint::Image::from_rgb8(pixel_buffer));
                        app.set_video_resolution(format!("{}x{}", width, height).into());
                    })
                    .unwrap();
            }
        },
        move |err_msg| {
            app_weak_for_error
                .upgrade_in_event_loop(move |app| {
                    use slint::Model;
                    app.set_stream_error(err_msg.clone().into());
                    let time_str = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

                    let current_logs = app.get_logs();
                    let mut logs_vec: Vec<LogMessage> = current_logs.iter().collect();
                    logs_vec.insert(
                        0,
                        LogMessage {
                            time: time_str.into(),
                            level: "ERROR".into(),
                            message: err_msg.into(),
                            detail: "播放器上报流媒体错误，系统已停止使用当前帧并等待下一次恢复。"
                                .into(),
                            image_url: "".into(),
                            snapshot: Default::default(),
                        },
                    );

                    let new_model = std::rc::Rc::new(slint::VecModel::from(logs_vec));
                    app.set_log_count(new_model.row_count() as i32);
                    app.set_logs(new_model.into());
                })
                .unwrap();
        },
    )?;

    app.run()?;

    Ok(())
}

fn load_http_image(url: &str) -> anyhow::Result<slint::Image> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let bytes = client.get(url).send()?.error_for_status()?.bytes()?;

    decode_image_bytes(&bytes)
}

fn decode_image_bytes(bytes: &[u8]) -> anyhow::Result<slint::Image> {
    let rgba_image = image::load_from_memory(bytes)?.into_rgba8();
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        rgba_image.as_raw(),
        rgba_image.width(),
        rgba_image.height(),
    );

    Ok(slint::Image::from_rgba8(buffer))
}

// Work around https://github.com/zmwangx/rust-ffmpeg/issues/102
#[derive(derive_more::Deref, derive_more::DerefMut)]
struct Rescaler(ffmpeg_next::software::scaling::Context);

unsafe impl std::marker::Send for Rescaler {}

fn rgb_rescaler_for_frame(frame: &ffmpeg_next::util::frame::Video) -> Rescaler {
    Rescaler(
        ffmpeg_next::software::scaling::Context::get(
            frame.format(),
            frame.width(),
            frame.height(),
            Pixel::RGB24,
            frame.width(),
            frame.height(),
            ffmpeg_next::software::scaling::Flags::BILINEAR,
        )
        .unwrap(),
    )
}

fn video_frame_to_pixel_buffer(
    frame: &ffmpeg_next::util::frame::Video,
) -> slint::SharedPixelBuffer<slint::Rgb8Pixel> {
    let mut pixel_buffer =
        slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(frame.width(), frame.height());

    let ffmpeg_line_iter = frame.data(0).chunks_exact(frame.stride(0));

    let slint_pixel_line_iter = pixel_buffer
        .make_mut_bytes()
        .chunks_mut(frame.width() as usize * core::mem::size_of::<slint::Rgb8Pixel>());

    for (source_line, dest_line) in ffmpeg_line_iter.zip(slint_pixel_line_iter) {
        dest_line.copy_from_slice(&source_line[..dest_line.len()]);
    }

    pixel_buffer
}

#[cfg(test)]
mod tests {
    #[test]
    fn decodes_encoded_image_bytes_to_slint_image() {
        let image = super::decode_image_bytes(b"P6\n1 1\n255\n\xff\x00\x00")
            .expect("PNM image should decode");
        let pixels = image
            .to_rgba8()
            .expect("decoded image should expose RGBA pixels");

        assert_eq!(pixels.width(), 1);
        assert_eq!(pixels.height(), 1);
    }
}
