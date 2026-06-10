slint::include_modules!();

use ffmpeg_next::format::Pixel;

mod player;

const VIDEO_URL: &str = "rtmp://127.0.0.1:1935/live/predict";

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,icu_provider=off,i_slint_core=off")).init();
    ffmpeg_next::init()?;

    let app = App::new()?;

    let url = std::path::PathBuf::from(VIDEO_URL);
    let mut to_rgb_rescaler: Option<Rescaler> = None;

    let logs_model = std::rc::Rc::new(slint::VecModel::default());
    logs_model.push(LogMessage {
        time: "2026-06-10 20:30:00".into(),
        level: "INFO".into(),
        message: "Edge node initialized successfully.".into(),
    });
    logs_model.push(LogMessage {
        time: "2026-06-10 20:35:12".into(),
        level: "WARN".into(),
        message: "High latency detected on stream.".into(),
    });
    app.set_logs(logs_model.clone().into());

    let app_weak_for_error = app.as_weak();

    let sysinfo_timer = slint::Timer::default();
    let sysinfo_app_weak = app.as_weak();
    let system = std::rc::Rc::new(std::cell::RefCell::new(sysinfo::System::new_all()));
    let networks = std::rc::Rc::new(std::cell::RefCell::new(sysinfo::Networks::new_with_refreshed_list()));

    sysinfo_timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(1), move || {
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
    });

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
            app_weak_for_error.upgrade_in_event_loop(move |app| {
                use slint::Model;
                app.set_stream_error(err_msg.clone().into());
                let time_str = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
                
                let current_logs = app.get_logs();
                let mut logs_vec: Vec<LogMessage> = current_logs.iter().collect();
                logs_vec.insert(0, LogMessage {
                    time: time_str.into(),
                    level: "ERROR".into(),
                    message: err_msg.into(),
                });
                
                let new_model = std::rc::Rc::new(slint::VecModel::from(logs_vec));
                app.set_logs(new_model.into());
            }).unwrap();
        }
    )?;

    app.run()?;

    Ok(())
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
