//! gaze-inject-cli: manual pointer/click/scroll injection through uinput, for exercising
//! the gaze-inject crate against a live cosmic-comp session. See `PLAN.md`'s gaze-inject
//! contract and `gaze_inject::Injector`'s doc comments for what each backend actually
//! does once cosmic-comp gets hold of its events.

use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use clap::{ArgGroup, Parser, ValueEnum};
use gaze_core::GlobalPx;
use gaze_inject::{Backend, Button, DeskLayout, Injector};

/// Outputs `--probe` walks, in the order named in `PLAN.md`'s "Environment facts".
const PROBE_OUTPUTS: [&str; 3] = ["DP-2", "DP-1", "HDMI-A-1"];

/// Pause between `--probe` moves: long enough for a human to look at the screen and note
/// which monitor the cursor landed on before the next move happens (still useful for the
/// absolute backend, which has no measured position to fall back on).
const PROBE_PAUSE: Duration = Duration::from_secs(3);

#[derive(Parser)]
#[command(author, version, about)]
#[command(group(
    ArgGroup::new("action")
        .args(["move_pos", "click", "scroll", "probe"])
        .required(true)
))]
struct Args {
    /// Which uinput device shape to create: a relative mouse (closed-loop against a
    /// measured cursor position when available, open-loop corner-homing otherwise), or an
    /// absolute ABS_X/ABS_Y device scaled to the desk layout's union bbox. See
    /// `gaze_inject`'s crate docs - the two are not equivalent under cosmic-comp today.
    #[arg(long, value_enum, default_value_t = BackendArg::Rel)]
    backend: BackendArg,

    /// Desk layout file (output logical rects). Defaults to config/desk.toml relative to
    /// the current directory, matching every other crate's bin.
    #[arg(long, default_value = "config/desk.toml")]
    desk_config: PathBuf,

    /// Moves the cursor to X Y (global logical px) and exits.
    #[arg(long = "move", num_args = 2, value_names = ["X", "Y"])]
    move_pos: Option<Vec<f64>>,

    /// Moves the cursor to X Y and clicks --button (default left).
    #[arg(long, num_args = 2, value_names = ["X", "Y"])]
    click: Option<Vec<f64>>,

    /// Which button --click reports.
    #[arg(long, value_enum, default_value_t = ButtonArg::Left)]
    button: ButtonArg,

    /// Moves the cursor to X Y and scrolls by DY wheel clicks (kernel REL_WHEEL
    /// convention: positive is up, away from the user).
    #[arg(long, num_args = 3, value_names = ["X", "Y", "DY"])]
    scroll: Option<Vec<f64>>,

    /// Walks the centre of every configured output, pausing 3 s between moves and
    /// printing which output it targets. With the relative backend's closed-loop tracker
    /// available, also prints the measured landing position and the error against the
    /// target, so confirming the mapping no longer requires eyeballing the screen. Never
    /// clicks.
    #[arg(long)]
    probe: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum BackendArg {
    Abs,
    Rel,
}

#[derive(Clone, Copy, ValueEnum)]
enum ButtonArg {
    Left,
    Right,
    Middle,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();

    let backend = match args.backend {
        BackendArg::Abs => Backend::Absolute,
        BackendArg::Rel => Backend::Relative,
    };

    let layout = DeskLayout::load(&args.desk_config)?;
    let mut injector = Injector::create_with(backend, &layout)?;

    if let Some(xy) = &args.move_pos {
        let p = GlobalPx { x: xy[0], y: xy[1] };
        injector.move_to(p)?;
        println!("moved to ({:.1}, {:.1})", p.x, p.y);
        print_measured_position(&mut injector)?;
    }
    else if let Some(xy) = &args.click {
        let p      = GlobalPx { x: xy[0], y: xy[1] };
        let button = match args.button {
            ButtonArg::Left   => Button::Left,
            ButtonArg::Right  => Button::Right,
            ButtonArg::Middle => Button::Middle,
        };

        injector.click_at(p, button)?;
        println!("clicked at ({:.1}, {:.1})", p.x, p.y);
    }
    else if let Some(xyz) = &args.scroll {
        let p  = GlobalPx { x: xyz[0], y: xyz[1] };
        let dy = xyz[2].round() as i32;

        injector.scroll(p, dy)?;
        println!("scrolled dy={dy} at ({:.1}, {:.1})", p.x, p.y);
    }
    else if args.probe {
        run_probe(&mut injector, &layout)?;
    }

    Ok(())
}

/// Prints the injector's last known measured position, if it has one. `--move`'s one-off
/// use; `run_probe` inlines the same query against each target so it can report the error
/// too.
fn print_measured_position(injector: &mut Injector) -> anyhow::Result<()> {
    if let Some(measured) = injector.last_known_position()? {
        println!("measured cursor position: ({:.1}, {:.1})", measured.x, measured.y);
    }

    Ok(())
}

/// Walks the centre of every output named in `PROBE_OUTPUTS`, pausing `PROBE_PAUSE`
/// between moves. Never emits a click or scroll event. Prints the measured landing
/// position and its error against the target when the backend can measure one (the
/// closed-loop relative backend); otherwise falls back to asking the human watching the
/// screen, since whether an unmeasured backend's cursor even lands on the right monitor
/// is an open question this crate could not settle from source alone (see the crate's
/// module docs).
fn run_probe(injector: &mut Injector, layout: &DeskLayout) -> anyhow::Result<()> {
    println!("gaze-inject-cli --probe: about to move the real cursor to the centre of each");
    println!(
        "configured output, one at a time, {} s apart. This never clicks.",
        PROBE_PAUSE.as_secs()
    );
    println!();

    for name in PROBE_OUTPUTS {
        let output = layout.get(name)?;
        let target = output.center();

        println!(
            "-> targeting {name} (centre of its logical rect, global px {:.0},{:.0})",
            target.x, target.y
        );
        injector.move_to(target)?;

        match injector.last_known_position()? {
            Some(measured) => {
                let err_x = measured.x - target.x;
                let err_y = measured.y - target.y;
                println!(
                    "   measured landing: ({:.1}, {:.1}), error ({:+.1}, {:+.1}) px",
                    measured.x, measured.y, err_x, err_y
                );
            }
            None => {
                println!("   no measured position available - watch the screen and note which");
                println!("   monitor the cursor actually appeared on");
            }
        }

        thread::sleep(PROBE_PAUSE);
    }

    println!();
    println!("probe complete.");

    Ok(())
}
