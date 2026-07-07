//! Headless visual verification for the Mission Control panel.
//!
//! Renders the panel's content view offscreen (no visible window, no screen
//! recording permission) and writes PNGs under `target/visual/`.
//!
//! Modes:
//! - no args:    fixtures -> target/visual/mission_control_fixtures.png
//! - `--filter`: fixtures with "Needs me" filter -> target/visual/mission_control_filtered.png
//! - `--live [path]`: real threads.db (read-only via temp copy)
//!                 -> target/visual/mission_control_live.png
//!
//! Run:
//!   cargo run -p agent_mission_control --bin mission_control_visual \
//!     --features "visual-tests,gpui_platform/runtime_shaders"

use std::path::PathBuf;
use std::sync::Arc;

use agent_mission_control::{MissionControlView, ThreadDigest, fixtures, store};
use gpui::{AppContext as _, HeadlessAppContext, px, size};

fn main() {
    env_logger::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let (digests, filter, output_name) = match args.first().map(String::as_str) {
        None => (
            fixtures::fixture_digests(),
            false,
            "mission_control_fixtures.png",
        ),
        Some("--filter") => (
            fixtures::fixture_digests(),
            true,
            "mission_control_filtered.png",
        ),
        Some("--live") => {
            let path = store::resolve_db_path(args.get(1).map(PathBuf::from));
            eprintln!("loading threads from {}", path.display());
            let digests = match store::load_digests(&path, store::DEFAULT_LIMIT) {
                Ok(digests) => digests,
                Err(error) => {
                    eprintln!("failed to load live threads db: {error:#}");
                    std::process::exit(1);
                }
            };
            eprintln!("loaded {} threads", digests.len());
            (digests, false, "mission_control_live.png")
        }
        Some(other) => {
            eprintln!("unknown argument: {other}");
            eprintln!("usage: mission_control_visual [--filter | --live [path]]");
            std::process::exit(2);
        }
    };

    let output_dir = PathBuf::from("target/visual");
    std::fs::create_dir_all(&output_dir).expect("creating target/visual");
    let output_path = output_dir.join(output_name);

    render_to_png(digests, filter, &output_path);

    let bytes = std::fs::metadata(&output_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    println!("wrote {} ({} bytes)", output_path.display(), bytes);
}

fn render_to_png(digests: Vec<ThreadDigest>, filter: bool, output_path: &std::path::Path) {
    let text_system = gpui_platform::current_platform(true).text_system();
    let mut cx = HeadlessAppContext::with_platform(text_system, Arc::new(assets::Assets), || {
        gpui_platform::current_headless_renderer()
    });

    cx.update(|cx| {
        assets::Assets
            .load_fonts(cx)
            .expect("loading embedded fonts");
        settings::init(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
    });
    cx.run_until_parked();

    let window = cx
        .open_window(size(px(420.), px(760.)), |_window, cx| {
            cx.new(|cx| {
                let mut view = MissionControlView::with_digests(digests, cx);
                if filter {
                    view.set_filter_needs_me(true, cx);
                }
                view
            })
        })
        .expect("opening headless window");

    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, _| window.refresh())
        .expect("refreshing window");
    cx.run_until_parked();

    let image = cx
        .capture_screenshot(window.into())
        .expect("capturing screenshot");
    println!("captured {}x{}", image.width(), image.height());
    image.save(output_path).expect("saving PNG");
}
