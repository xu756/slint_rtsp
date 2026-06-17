slint::include_modules!();

use ffmpeg_next::format::Pixel;
use slint::Model;

mod player;

const VIDEO_URL: &str = "rtmp://127.0.0.1:1935/live/predict";
const DISPLAY_CAMERA_URL: &str = "rtsp://192.168.8.41:8554/fridge/cabinet-a";
const LOG_SNAPSHOT_URL: &str = "https://picsum.photos/seed/fridge-log/720/405";
const ALARM_SNAPSHOT_URL: &str = "https://picsum.photos/seed/fridge-alarm/720/405";

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
    app.on_quit_requested(|| {
        slint::quit_event_loop().unwrap();
    });
    app.set_video_url(DISPLAY_CAMERA_URL.into());

    let url = std::path::PathBuf::from(VIDEO_URL);
    let mut to_rgb_rescaler: Option<Rescaler> = None;
    let log_snapshot = load_http_image(LOG_SNAPSHOT_URL).unwrap_or_default();
    let alarm_snapshot = load_http_image(ALARM_SNAPSHOT_URL).unwrap_or_default();

    let logs_model = std::rc::Rc::new(slint::VecModel::from(demo_logs(log_snapshot.clone())));
    app.set_logs(logs_model.clone().into());
    app.set_log_count(logs_model.row_count() as i32);

    let alarms_model = std::rc::Rc::new(slint::VecModel::from(demo_alarms(alarm_snapshot.clone())));
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
                            message: "柜内摄像头画面中断".into(),
                            detail: format!(
                                "冷柜 A-01 的柜内摄像头连接异常，温控、门磁、压缩机等传感器数据仍在继续采集。原始错误: {err_msg}"
                            )
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

fn demo_logs(snapshot: slint::Image) -> Vec<LogMessage> {
    vec![
        LogMessage {
            time: "08:30:02".into(),
            level: "INFO".into(),
            message: "冷藏室温度稳定在 3.8C".into(),
            detail: "冷藏室最近 15 分钟温度波动范围 3.6C - 4.0C，压缩机按节能曲线间歇运行，食品展示区处于正常保鲜状态。".into(),
            image_url: LOG_SNAPSHOT_URL.into(),
            snapshot: snapshot.clone(),
        },
        LogMessage {
            time: "08:42:18".into(),
            level: "INFO".into(),
            message: "冷冻室温度稳定在 -18.6C".into(),
            detail: "冷冻室温度达到设定目标，蒸发器风机运行正常，未检测到异常结霜或传感器漂移。".into(),
            image_url: LOG_SNAPSHOT_URL.into(),
            snapshot: snapshot.clone(),
        },
        LogMessage {
            time: "09:05:41".into(),
            level: "DEBUG".into(),
            message: "压缩机进入低功耗巡航".into(),
            detail: "压缩机连续运行 12 分钟后切换到低功耗巡航，当前电流 1.8A，预计 6 分钟后再次采样判断。".into(),
            image_url: LOG_SNAPSHOT_URL.into(),
            snapshot: snapshot.clone(),
        },
        LogMessage {
            time: "09:21:07".into(),
            level: "INFO".into(),
            message: "门磁检测: 关门确认".into(),
            detail: "左侧展示门关闭到位，门封压力正常，未检测到冷气泄漏趋势。".into(),
            image_url: LOG_SNAPSHOT_URL.into(),
            snapshot: snapshot.clone(),
        },
        LogMessage {
            time: "10:00:00".into(),
            level: "INFO".into(),
            message: "自动除霜计划完成".into(),
            detail: "蒸发器除霜周期结束，回温时间 4 分 20 秒，排水盘液位正常，制冷已恢复。".into(),
            image_url: LOG_SNAPSHOT_URL.into(),
            snapshot: snapshot.clone(),
        },
        LogMessage {
            time: "10:18:33".into(),
            level: "WARN".into(),
            message: "冷藏室湿度偏高 78%".into(),
            detail: "冷藏室湿度高于建议范围，可能与频繁开门或热食放入有关。系统已提高风机转速并持续观察。".into(),
            image_url: LOG_SNAPSHOT_URL.into(),
            snapshot,
        },
    ]
}

fn demo_alarms(snapshot: slint::Image) -> Vec<AlarmMessage> {
    vec![
        AlarmMessage {
            time: "10:22:15".into(),
            level: "高危".into(),
            message: "柜门长时间未关闭".into(),
            status: "未处理".into(),
            detail: "A-01 左侧展示门已打开 126 秒，冷藏室温度从 3.8C 升至 6.2C。请立即确认门体是否关严，避免食品温度超标。".into(),
            image_url: ALARM_SNAPSHOT_URL.into(),
            snapshot: snapshot.clone(),
        },
        AlarmMessage {
            time: "10:19:48".into(),
            level: "中危".into(),
            message: "冷藏室温度接近上限".into(),
            status: "未处理".into(),
            detail: "冷藏室当前温度 7.6C，接近设定上限 8.0C。系统已提升压缩机输出并建议检查门封、装载量和出风口遮挡情况。".into(),
            image_url: ALARM_SNAPSHOT_URL.into(),
            snapshot: snapshot.clone(),
        },
        AlarmMessage {
            time: "09:58:02".into(),
            level: "中危".into(),
            message: "蒸发器结霜偏厚".into(),
            status: "处理中".into(),
            detail: "蒸发器温差异常，疑似结霜偏厚。系统已安排一次短除霜周期，完成后复测制冷效率。".into(),
            image_url: ALARM_SNAPSHOT_URL.into(),
            snapshot: snapshot.clone(),
        },
        AlarmMessage {
            time: "09:33:27".into(),
            level: "低危".into(),
            message: "库存托盘摆放遮挡出风口".into(),
            status: "已处理".into(),
            detail: "柜内摄像头识别到第二层托盘靠近出风口，可能影响温度均匀性。已记录现场处理结果。".into(),
            image_url: ALARM_SNAPSHOT_URL.into(),
            snapshot: snapshot.clone(),
        },
        AlarmMessage {
            time: "08:47:10".into(),
            level: "低危".into(),
            message: "冷凝器清洁周期提醒".into(),
            status: "已处理".into(),
            detail: "冷凝器累计运行 168 小时，达到清洁提醒周期。建议巡检滤网和散热片积尘情况。".into(),
            image_url: ALARM_SNAPSHOT_URL.into(),
            snapshot,
        },
    ]
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

    #[test]
    fn demo_events_use_refrigerator_context() {
        let logs = super::demo_logs(slint::Image::default());
        let alarms = super::demo_alarms(slint::Image::default());

        assert!(logs.iter().any(|log| log.message.contains("冷藏室")));
        assert!(logs.iter().any(|log| log.detail.contains("压缩机")));
        assert!(alarms.iter().any(|alarm| alarm.message.contains("门")));
        assert!(alarms.iter().any(|alarm| alarm.detail.contains("温度")));
    }
}
