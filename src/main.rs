use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use filemgr::app::{run_script, App};
use filemgr::config::{self, Config};
use filemgr::term;

#[derive(Parser)]
#[command(
    name = "fm",
    version,
    about = "Terminal file manager: Miller columns, previews, background copy and move, search and bookmarks"
)]
struct Cli {
    /// Folder to open (or a file, to open its folder with the file selected); default: the current folder
    path: Option<PathBuf>,
    /// On quit, write the folder you were in to this file (for a shell function that changes to it)
    #[arg(long, value_name = "FILE")]
    cd_file: Option<PathBuf>,
    /// Configuration folder (holds config.toml and bookmarks); default: see the README
    #[arg(long, value_name = "DIR")]
    config: Option<PathBuf>,
    /// Ignore any configuration file and bookmarks
    #[arg(long)]
    no_config: bool,
    /// Replay comma-separated keys without a terminal and print the final screen (see the README)
    #[arg(long, value_name = "KEYS")]
    keys: Option<String>,
    /// Screen size for --keys, as WIDTHxHEIGHT
    #[arg(long, value_name = "WxH", default_value = "100x24")]
    screen: String,
    /// With --keys, print ANSI colours
    #[arg(long)]
    ansi: bool,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fm: {e}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let cfg = if cli.no_config {
        Config::default()
    } else {
        match cli.config.clone().or_else(config::config_dir) {
            Some(dir) => Config::load(&dir)?,
            None => Config::default(),
        }
    };
    let start = cli.path.clone().unwrap_or_else(|| PathBuf::from("."));
    let mut app = App::new(&start, cfg)?;
    if let Some(keys) = &cli.keys {
        let (w, h) = cli
            .screen
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse::<usize>().ok()?, h.parse::<usize>().ok()?)))
            .ok_or("--screen must look like 100x24")?;
        let screen = run_script(&mut app, keys, w, h)?;
        println!("{}", if cli.ansi { screen.ansi() } else { screen.plain() });
    } else {
        if !std::io::stdout().is_terminal() {
            return Err("standard output is not a terminal (use --keys to replay keys and print the screen)".into());
        }
        term::run(&mut app).map_err(|e| e.to_string())?;
    }
    if let Some(f) = &cli.cd_file {
        std::fs::write(f, app.cwd.display().to_string()).map_err(|e| format!("{}: {e}", f.display()))?;
    }
    Ok(())
}
