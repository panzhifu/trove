//! `trove` — command-line access to a Trove library.
//!
//! The desktop app is for looking at an asset library; this binary is for
//! everything that has to happen *without* a window: a script filing a
//! screenshot, an agent answering "what is in the library", a cron job
//! sweeping an inbox folder. It talks to the same `trove-core` the app does,
//! so it sees the same database, the same index and the same rules — and it
//! can run while the app is open, because it opens the library read-only.
//!
//! Two conventions hold everywhere, and both exist for a caller that is not a
//! human: **stdout is one JSON document**, and **the exit status is the only
//! thing that decides whether to parse it** (0 ok, 1 failed, 2 misuse, 3
//! library unusable). `--human` swaps the JSON for tables when a person is
//! reading.

mod cli;
mod ctx;
mod read;
mod write;

use clap::Parser;

use cli::{Cli, Command};
use ctx::{CliError, Env, Style};

fn main() {
    let args = Cli::parse();
    init_logging(&args);
    let code = match dispatch(&args) {
        Ok(()) => 0,
        Err(error) => {
            error.report();
            error.code()
        }
    };
    std::process::exit(code);
}

fn dispatch(args: &Cli) -> Result<(), CliError> {
    let style = Style::new(args);

    let rendered = match &args.command {
        // Commands that read configuration only. They must not need a usable
        // library: "which library is broken" is exactly the question they
        // answer.
        Command::Libraries => read::libraries()?,
        Command::Paths => read::paths(args)?,

        command => {
            let env = Env::open(args)?;
            match command {
                Command::Info => read::info(&env)?,
                Command::List(list) => read::list(&env, list)?,
                Command::Search(search) => read::search(&env, search, &style)?,
                Command::Get(get) => read::get(&env, get)?,
                Command::Tags => read::tags(&env)?,
                Command::Collections => read::collections(&env)?,
                Command::Duplicates => read::duplicates(&env)?,
                Command::Folders => read::folders(&env)?,
                Command::Doctor => read::doctor(&env)?,
                Command::Import(import) => write::import(&env, import, &style)?,
                Command::Autotag(autotag) => write::autotag(&env, autotag, &style)?,
                Command::Set(set) => write::set(&env, set)?,
                Command::Tag(tag) => write::tag(&env, tag)?,
                Command::Trash(ids) => write::trash(&env, ids)?,
                Command::Restore(ids) => write::restore(&env, ids)?,
                Command::Purge(purge) => write::purge(&env, purge)?,
                Command::Collection(collection) => write::collection(&env, collection)?,
                Command::Index(index) => write::index(&env, index)?,
                Command::Libraries | Command::Paths => {
                    unreachable!("handled above, before a library is opened")
                }
            }
        }
    };

    style.emit(&rendered)
}

/// Log to stderr, quietly by default.
///
/// The library is chatty at `info` (drains, thumbnails, watches) and none of
/// that belongs in a script's output, so the default is `warn` and `RUST_LOG`
/// is there when something needs chasing. `--quiet` drops to errors only.
fn init_logging(args: &Cli) {
    use tracing_subscriber::EnvFilter;

    let default = if args.quiet { "error" } else { "warn" };
    let filter = match std::env::var("RUST_LOG") {
        Ok(spec) if !spec.trim().is_empty() => EnvFilter::new(spec),
        _ => EnvFilter::new(default),
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}
