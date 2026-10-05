mod menu;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use wdfr::carve::{self, Category};
use wdfr::filter::Filter;
use wdfr::recover::{self, Method, Options, Session};
use wdfr::units::{format_size, parse_size};
use wdfr::{devices, fs};

/// Recover deleted photos, videos, documents and other files from NTFS,
/// FAT12/16/32 and exFAT volumes, whole disks, memory cards and disk images.
///
/// Run without arguments for an easy, menu-driven mode.
///
/// The source is only ever opened read-only. Always write recovered files
/// to a different drive than the one you are recovering from.
#[derive(Parser)]
#[command(name = "wdfr", version, about, long_about, propagate_version = true, after_help = menu::POWERED_BY)]
struct Cli {
    /// More logging (-v info, -vv debug).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Leave out to start the interactive menu.
    #[command(subcommand)]
    command: Option<Command>,
}

/// Set by Ctrl+C while a recovery is running: finish the current file,
/// write the report and stop.
static CANCEL: AtomicBool = AtomicBool::new(false);
/// True while a recovery is running; otherwise Ctrl+C exits at once.
static BUSY: AtomicBool = AtomicBool::new(false);

fn install_ctrlc_handler() {
    let _ = ctrlc::set_handler(|| {
        if !BUSY.load(Ordering::SeqCst) || CANCEL.swap(true, Ordering::SeqCst) {
            let _ = console::Term::stdout().show_cursor();
            std::process::exit(130);
        }
        eprintln!("\nStopping after the current file (Ctrl+C again to abort immediately)...");
    });
}

#[derive(Subcommand)]
enum Command {
    /// List disks and volumes that can be scanned.
    Devices,
    /// Show the partitions and file systems on a source.
    Info {
        /// Device (E:, \\.\PhysicalDrive1, /dev/disk4, /dev/sdb) or image file.
        source: String,
    },
    /// List deleted files found in file-system metadata (writes nothing).
    Scan {
        /// Device (E:, \\.\PhysicalDrive1, /dev/disk4, /dev/sdb) or image file.
        source: String,
        /// Only this partition (number from `wdfr info`).
        #[arg(short, long)]
        partition: Option<usize>,
        #[command(flatten)]
        filter: FilterArgs,
        /// Hide files whose data has been overwritten.
        #[arg(long)]
        recoverable_only: bool,
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Recover deleted files into an output directory.
    Recover {
        /// Device (E:, \\.\PhysicalDrive1, /dev/disk4, /dev/sdb) or image file.
        source: String,
        /// Output directory. Must be on a different drive than the source.
        #[arg(short, long)]
        output: PathBuf,
        /// Recovery strategy.
        #[arg(short, long, value_enum, default_value_t = Method::All)]
        method: Method,
        /// Only this partition (number from `wdfr info`).
        #[arg(short, long)]
        partition: Option<usize>,
        #[command(flatten)]
        filter: FilterArgs,
        /// Also write files that appear overwritten (usually garbage).
        #[arg(long)]
        include_overwritten: bool,
        /// Carve the entire disk, not only unallocated space (finds copies
        /// of existing files too).
        #[arg(long)]
        carve_all_space: bool,
        /// Look for files at every byte instead of every 512-byte sector, to
        /// find files embedded inside other data. CPU-bound (~150 MB/s)
        /// instead of disk-bound.
        #[arg(long)]
        deep: bool,
        /// Upper size limit for any carved file (e.g. 4G).
        #[arg(long, value_parser = parse_size)]
        max_carve_size: Option<u64>,
        /// Allow writing to the volume being recovered (dangerous).
        #[arg(long)]
        allow_same_volume: bool,
        /// No progress bars.
        #[arg(short, long)]
        quiet: bool,
    },
    /// List the file formats the carver understands.
    Formats,
}

#[derive(Args)]
struct FilterArgs {
    /// Only these extensions, comma separated (e.g. jpg,png,mp4).
    #[arg(short = 't', long = "type", value_delimiter = ',')]
    types: Vec<String>,
    /// Only these categories: image, video, audio, document, archive, database.
    #[arg(short, long, value_delimiter = ',')]
    category: Vec<Category>,
    /// Glob on the file name, or on the full path if it contains '/'
    /// (e.g. "IMG_*", "Users/*/Pictures/**").
    #[arg(short, long)]
    name: Option<String>,
    /// Minimum file size (e.g. 10K, 1.5M).
    #[arg(long, value_parser = parse_size, default_value = "0")]
    min_size: u64,
    /// Maximum file size (e.g. 2G).
    #[arg(long, value_parser = parse_size)]
    max_size: Option<u64>,
}

impl FilterArgs {
    fn build(&self) -> Result<Filter> {
        Filter::new(&self.types, &self.category, self.name.as_deref(), self.min_size, self.max_size)
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let level = match cli.verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level)).format_timestamp(None).init();
    install_ctrlc_handler();
    let result = match cli.command {
        Some(cmd) => run(cmd),
        // No arguments: guided menu when a person is at the keyboard.
        None if console::user_attended() => menu::run(),
        None => {
            use clap::CommandFactory;
            Cli::command().print_long_help().map(|_| ExitCode::SUCCESS).map_err(Into::into)
        }
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cmd: Command) -> Result<ExitCode> {
    match cmd {
        Command::Devices => cmd_devices(),
        Command::Formats => cmd_formats(),
        Command::Info { source } => cmd_info(&source),
        Command::Scan { source, partition, filter, recoverable_only, json } => {
            cmd_scan(&source, partition, &filter.build()?, recoverable_only, json)
        }
        Command::Recover {
            source,
            output,
            method,
            partition,
            filter,
            include_overwritten,
            carve_all_space,
            deep,
            max_carve_size,
            allow_same_volume,
            quiet,
        } => {
            let opts = Options {
                out: output,
                method,
                partition,
                filter: filter.build()?,
                include_overwritten,
                carve_all_space,
                step: if deep { 1 } else { 512 },
                max_carve_size,
                allow_same_volume,
                quiet,
            };
            cmd_recover(&source, &opts)
        }
    }
}

fn cmd_devices() -> Result<ExitCode> {
    let list = devices::list();
    if list.is_empty() {
        println!("No devices found.");
    }
    println!("{:<28} {:>12}  DESCRIPTION", "PATH", "SIZE");
    for d in &list {
        let size = d.size.map_or_else(|| "no access".to_string(), format_size);
        println!("{:<28} {:>12}  {}", d.path, size, d.description);
    }
    if list.iter().any(|d| d.size.is_none()) {
        println!("\nSome devices could not be opened: run as Administrator (Windows) or with sudo.");
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_formats() -> Result<ExitCode> {
    println!("{:<10} {:<24} EXTENSIONS", "FORMAT", "CATEGORIES");
    for f in carve::all_formats() {
        let exts: Vec<_> = f.kinds().iter().map(|(e, _)| *e).collect();
        let mut cats: Vec<String> = Vec::new();
        for (_, c) in f.kinds() {
            if !cats.contains(&c.to_string()) {
                cats.push(c.to_string());
            }
        }
        println!("{:<10} {:<24} {}", f.name(), cats.join(","), exts.join(", "));
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_info(source: &str) -> Result<ExitCode> {
    let s = Session::open(source)?;
    println!("Source: {} ({})", s.path, format_size(s.disk.size()));
    println!("{:>3}  {:>14}  {:>12}  {:<7} {:<24} DETAILS", "#", "OFFSET", "SIZE", "FS", "TYPE");
    for p in &s.partitions {
        let details = match p.fs {
            Some(_) => match fs::open(p.source(&s.disk)) {
                Ok(v) => v.describe(),
                Err(e) => format!("unreadable: {e:#}"),
            },
            None => "no supported file system (carving only)".into(),
        };
        let kind = if p.name.is_empty() { p.kind.clone() } else { format!("{} \"{}\"", p.kind, p.name) };
        println!(
            "{:>3}  {:>14}  {:>12}  {:<7} {:<24} {}",
            p.index,
            format!("{:#x}", p.start),
            format_size(p.len),
            p.fs.map_or_else(|| "-".into(), |f| f.to_string()),
            kind,
            details
        );
    }
    let gaps = wdfr::partition::unpartitioned(s.disk.size(), &s.partitions);
    let gap_bytes = wdfr::ranges::total(&gaps);
    if gap_bytes >= 1 << 20 {
        println!("\nUnpartitioned space: {} (carved by `recover`)", format_size(gap_bytes));
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_scan(
    source: &str,
    partition: Option<usize>,
    filter: &Filter,
    recoverable_only: bool,
    json: bool,
) -> Result<ExitCode> {
    let s = Session::open(source)?;
    let mut all = Vec::new();
    for p in s.selected(partition)? {
        match recover::scan_partition(&s, p, filter, json)? {
            Some((_, mut files)) => {
                if recoverable_only {
                    files.retain(|f| f.condition.is_recoverable());
                }
                files.sort_by(|a, b| a.path.cmp(&b.path));
                all.push((p.clone(), files));
            }
            None => {
                if !json {
                    eprintln!("{}: no supported file system; use `recover --method carve`", p.label());
                }
            }
        }
    }
    if json {
        let v: Vec<_> = all.iter().map(|(p, files)| serde_json::json!({ "partition": p, "files": files })).collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(ExitCode::SUCCESS);
    }
    for (p, files) in &all {
        println!("\n== {} ({} deleted files) ==", p.label(), files.len());
        if files.is_empty() {
            continue;
        }
        println!("{:<22} {:>10}  {:<19}  PATH", "CONDITION", "SIZE", "MODIFIED");
        for f in files {
            let modified = f.modified.map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default();
            let note = f.note.as_deref().map(|n| format!("  [{n}]")).unwrap_or_default();
            println!(
                "{:<22} {:>10}  {:<19}  {}{}",
                f.condition.to_string(),
                format_size(f.size),
                modified,
                f.path,
                note
            );
        }
        let ok = files.iter().filter(|f| f.condition.is_recoverable()).count();
        println!("{ok} of {} look recoverable.", files.len());
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_recover(source: &str, opts: &Options) -> Result<ExitCode> {
    let s = Session::open(source)?;
    if !opts.quiet {
        eprintln!("wdfr {} - {}", env!("CARGO_PKG_VERSION"), menu::POWERED_BY);
        eprintln!("Source: {} ({})", s.path, format_size(s.disk.size()));
        for p in s.selected(opts.partition)? {
            eprintln!("  {} at {:#x}, {}", p.label(), p.start, format_size(p.len));
        }
    }
    CANCEL.store(false, Ordering::SeqCst);
    BUSY.store(true, Ordering::SeqCst);
    let sum = recover::run(&s, opts, &CANCEL);
    BUSY.store(false, Ordering::SeqCst);
    let sum = sum?;
    println!();
    println!("Recovered from file system: {} files ({})", sum.fs_files, format_size(sum.fs_bytes));
    println!("Recovered by carving:       {} files ({})", sum.carved_files, format_size(sum.carved_bytes));
    if sum.skipped_overwritten > 0 {
        println!(
            "Skipped (overwritten):      {} files (use --include-overwritten to write them anyway)",
            sum.skipped_overwritten
        );
    }
    if sum.unreadable_bytes > 0 {
        println!("Unreadable (zero-filled):   {}", format_size(sum.unreadable_bytes));
    }
    if sum.failures > 0 {
        println!("Failed to write:            {} files (see messages above)", sum.failures);
    }
    if let Some(r) = &sum.report {
        println!("Report:                     {}", r.display());
    }
    if sum.cancelled {
        println!("Interrupted: results are partial.");
        return Ok(ExitCode::from(130));
    }
    Ok(ExitCode::SUCCESS)
}
