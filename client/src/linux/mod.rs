//! The Linux/Wayland client: `transom-client run [<host>]`.
//!
//! The counterpart of the Windows window manager (`win`). It speaks the same wire
//! protocol through the shared pure core (`wire`, `model`, `session`), turns each
//! Mac window into a native Wayland toplevel (winit), draws the shared decoded
//! texture 1:1 with a point-fetched wgpu blit, and decodes HEVC with libavcodec.
//!
//! ## What differs from Windows, and why
//!
//!  * **Scale.** Wayland scale is per-output and may be fractional. winit hands us
//!    physical pixels and maps the physical buffer onto the logical surface with
//!    `wp_viewporter`, so physical == buffer pixels and I-2 holds. A scale change
//!    keeps the *physical* size (see `app::proxy_event`).
//!  * **Resize.** There is no `WM_ENTERSIZEMOVE`/`EXITSIZEMOVE`. `resize` infers a
//!    drag from configure events and ends it when they stop.
//!  * **Positioning.** Clients cannot place windows; the compositor does.
//!  * **Input.** Keys are mapped by physical position to Windows VK codes
//!    (`input`); there is no global shortcut, so the disconnect chord works while
//!    a Transom window has focus.
//!  * **Hiding.** Wayland toplevels cannot be hidden, so closing a proxy destroys
//!    it and reopening from the dashboard creates a fresh one.
//!
//! Like `win`, this half can only be verified on a real session: the pure logic
//! (`resize`, `input`, thumbnails) is unit-tested, the GPU/compositor behaviour is
//! checked by `transom-client doctor` and by running against a host.

mod app;
mod dashboard;
mod decode;
pub mod doctor;
mod gpu;
mod input;
mod proxy;
mod resize;

use std::process::ExitCode;

use crate::wire::{DEFAULT_CONTROL_PORT, DEFAULT_VIDEO_PORT};
use app::AppConfig;

struct Args {
    host: String,
    control_port: u16,
    video_port: Option<u16>,
    checkerboard: bool,
    open_all: bool,
}

pub fn run(args: &[String]) -> ExitCode {
    let cfg = if args.is_empty() || args == ["--interactive"] {
        None
    } else {
        match parse(args) {
            Ok(a) => Some(AppConfig {
                host: a.host,
                control_port: a.control_port,
                video_port: a.video_port,
                checkerboard: a.checkerboard,
                open_all: a.open_all,
            }),
            Err(msg) => {
                eprintln!("transom-client: {msg}\n");
                print_usage();
                return ExitCode::FAILURE;
            }
        }
    };
    match app::run(cfg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("transom-client: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut host: Option<String> = None;
    let mut control_port = DEFAULT_CONTROL_PORT;
    // Video is ON by default: `run` is the real window manager, and with video off
    // every window shows only the placeholder. `--no-video` opts out for
    // control-plane-only debugging; the video channel is best-effort anyway.
    let mut video_port: Option<u16> = Some(DEFAULT_VIDEO_PORT);
    let mut checkerboard = false;
    let mut open_all = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--control-port" => {
                i += 1;
                control_port = args
                    .get(i)
                    .ok_or("--control-port needs a value")?
                    .parse()
                    .map_err(|_| "invalid --control-port")?;
            }
            "--video" => {
                video_port.get_or_insert(DEFAULT_VIDEO_PORT);
            }
            "--no-video" => video_port = None,
            "--video-port" => {
                i += 1;
                video_port = Some(
                    args.get(i)
                        .ok_or("--video-port needs a value")?
                        .parse()
                        .map_err(|_| "invalid --video-port")?,
                );
            }
            "--checkerboard" => checkerboard = true,
            "--open-all" => open_all = true,
            other if other.starts_with('-') => return Err(format!("unknown option {other}")),
            _ => {
                if host.is_some() {
                    return Err("unexpected extra argument".into());
                }
                host = Some(args[i].clone());
            }
        }
        i += 1;
    }

    let host = host.ok_or("missing Mac hostname or IP address")?;
    crate::connections::Connection::manual(
        &host,
        &control_port.to_string(),
        &video_port.map(|p| p.to_string()).unwrap_or_default(),
    )?;
    Ok(Args {
        host,
        control_port,
        video_port,
        checkerboard,
        open_all,
    })
}

fn print_usage() {
    println!(
        "transom-client run — manage Mac windows as native Wayland proxy windows\n\
         \n\
         USAGE:\n    \
         transom-client run [<host>] [options]\n\
         \n\
         OPTIONS:\n    \
         --interactive        open the dashboard (also used when host is omitted)\n    \
         --control-port <n>   control channel port (default 47100)\n    \
         --video              open the video channel on the default port (47101) [on by default]\n    \
         --no-video           control-plane only; every window shows the placeholder\n    \
         --video-port <n>     open the video channel on <n>\n    \
         --checkerboard       draw a 1px checkerboard test pattern in each window (M0 probe)\n    \
         --open-all           open every shared window as it appears, skipping the dashboard card\n\
         \n\
         ENVIRONMENT:\n    \
         TRANSOM_DECODE_THREADS=<n>   HEVC decoder threads (default min(cores, 4); 1 = lowest latency)\n\
         \n\
         EXAMPLES:\n    \
         transom-client run                         # open the dashboard\n    \
         transom-client run 192.168.1.20             # connect directly\n    \
         transom-client run 192.168.1.20 --no-video"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn host_alone_uses_default_ports_with_video() {
        let a = parse(&args(&["192.168.1.20"])).unwrap();
        assert_eq!(a.host, "192.168.1.20");
        assert_eq!(a.control_port, DEFAULT_CONTROL_PORT);
        assert_eq!(a.video_port, Some(DEFAULT_VIDEO_PORT));
        assert!(!a.checkerboard);
    }

    #[test]
    fn options_override_ports_and_disable_video() {
        let a = parse(&args(&["mac.local", "--control-port", "5000", "--no-video"])).unwrap();
        assert_eq!((a.control_port, a.video_port), (5000, None));
        let a = parse(&args(&["mac.local", "--video-port", "5001", "--checkerboard"])).unwrap();
        assert_eq!(a.video_port, Some(5001));
        assert!(a.checkerboard);
    }

    #[test]
    fn rejects_missing_host_unknown_options_and_equal_ports() {
        assert!(parse(&args(&[])).is_err());
        assert!(parse(&args(&["--no-video"])).is_err());
        assert!(parse(&args(&["a", "--bogus"])).is_err());
        assert!(parse(&args(&["a", "b"])).is_err());
        assert!(parse(&args(&["a", "--control-port", "5000", "--video-port", "5000"])).is_err());
    }
}
