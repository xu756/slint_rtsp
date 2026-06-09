slint::include_modules!();

use ffmpeg_next::format::Pixel;

mod player;

const VIDEO_URL: &str = "rtmp://127.0.0.1:1935/live/predict";

fn main() -> anyhow::Result<()> {
    ffmpeg_next::init()?;

    let app = App::new()?;

    let url = std::path::PathBuf::from(VIDEO_URL);
    let mut to_rgb_rescaler: Option<Rescaler> = None;

    let mut player = player::Player::start(
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

                app_weak
                    .upgrade_in_event_loop(move |app| {
                        app.set_video_frame(slint::Image::from_rgb8(pixel_buffer));
                    })
                    .unwrap();
            }
        },
        {
            let app_weak = app.as_weak();

            move |playing| {
                app_weak
                    .upgrade_in_event_loop(move |app| {
                        app.set_playing(playing);
                    })
                    .unwrap();
            }
        },
    )?;

    app.on_toggle_pause_play(move || {
        player.toggle_pause_playing();
    });

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
