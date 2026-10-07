//! Interactive, menu-driven mode: started when `wdfr` is run without
//! arguments (e.g. by double-clicking `wdfr.exe`). Every choice is made from
//! a list, so no command-line syntax is needed.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;
use console::style;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, MultiSelect, Select};

use wdfr::carve::Category;
use wdfr::filter::Filter;
use wdfr::recover::{Method, Options, Session};
use wdfr::source::{DiskSource, Source};
use wdfr::units::format_size;
use wdfr::{devices, partition};

pub const POWERED_BY: &str = "Powered by Bashar Salmo";

pub fn banner() {
    let line = "=".repeat(64);
    println!();
    println!("  {}", style(&line).cyan());
    println!(
        "    {}  {}",
        style("Deleted Files Recovery").bold(),
        style(concat!("v", env!("CARGO_PKG_VERSION"))).dim()
    );
    println!("    Recover deleted photos, videos, documents and more");
    println!("    {}", style(POWERED_BY).yellow().bold());
    println!("  {}", style(&line).cyan());
    println!();
}

pub fn run() -> Result<ExitCode> {
    let theme = ColorfulTheme::default();
    loop {
        banner();
        let choice = Select::with_theme(&theme)
            .with_prompt("What would you like to do?")
            .items([
                "Recover deleted files",
                "Preview deleted files (scan only, nothing is saved)",
                "Show drive information",
                "List supported file types",
                "Read this first: tips for a successful recovery",
                "Exit",
            ])
            .default(0)
            .interact_opt()?;
        let result = match choice {
            Some(0) => recover_wizard(&theme, None),
            Some(1) => preview(&theme),
            Some(2) => drive_info(&theme),
            Some(3) => crate::cmd_formats().map(|_| ()),
            Some(4) => {
                tips();
                Ok(())
            }
            _ => break,
        };
        if let Err(e) = result {
            println!("\n{} {e:#}", style("Error:").red().bold());
        }
        pause()?;
    }
    println!("\n  {}\n", style(POWERED_BY).yellow());
    Ok(ExitCode::SUCCESS)
}

fn pause() -> Result<()> {
    Input::<String>::new()
        .with_prompt(format!("{}", style("Press Enter to return to the main menu").dim()))
        .allow_empty(true)
        .report(false)
        .interact_text()?;
    Ok(())
}

fn header(title: &str) {
    println!("\n  {}\n", style(title).bold().underlined());
}

/// Lets the user pick a drive or image. `None` = went back.
fn pick_source(theme: &ColorfulTheme) -> Result<Option<String>> {
    println!("  Looking for drives...");
    let devs = devices::list();
    // The manual entry comes first so it is never buried below a long list.
    let mut items = vec!["Type a drive letter, device or disk image path...".to_string()];
    let mut paths = Vec::new();
    for d in &devs {
        let detail = match d.size {
            Some(size) => format!("{:>10}  {}", format_size(size), describe(&d.path)),
            None => format!("{:>10}  needs administrator rights", "-"),
        };
        items.push(format!("{:<24} {}  ({})", d.path, detail, d.description));
        paths.push(d.path.clone());
    }
    if devs.iter().all(|d| d.size.is_none()) {
        println!(
            "  {}",
            style(if cfg!(windows) {
                "Tip: no drive could be opened. Close this window, right-click wdfr.exe and choose \"Run as administrator\"."
            } else {
                "Tip: no drive could be opened. Run wdfr with sudo to scan drives."
            })
            .yellow()
        );
    }
    let back = items.len();
    items.push("<- Back".into());
    // Start on the first drive that can actually be opened.
    let default = devs.iter().position(|d| d.size.is_some()).map_or(0, |i| i + 1);
    let choice = Select::with_theme(theme)
        .with_prompt("Which drive did the files get deleted from?")
        .items(&items)
        .default(default)
        .max_length(15)
        .interact_opt()?;
    match choice {
        Some(0) => {
            let p: String = Input::with_theme(theme)
                .with_prompt(if cfg!(windows) {
                    "Drive letter (e.g. E:) or image file path"
                } else {
                    "Device or image file path"
                })
                .interact_text()?;
            let p = p.trim().trim_matches('"').to_string();
            Ok((!p.is_empty()).then_some(p))
        }
        Some(i) if i < back => Ok(Some(paths[i - 1].clone())),
        _ => Ok(None),
    }
}

/// "NTFS", "exFAT", "2 partitions: NTFS, FAT32", ... (best effort, fast).
fn describe(path: &str) -> String {
    let Ok(d) = DiskSource::open(path) else { return String::new() };
    let disk: Source = std::sync::Arc::new(d);
    let parts = partition::discover(&disk);
    let fs: Vec<String> = parts.iter().map(|p| p.fs.map_or_else(|| "unknown".into(), |f| f.to_string())).collect();
    match fs.len() {
        0 => String::new(),
        1 => fs[0].clone(),
        n => format!("{n} partitions: {}", fs.join(", ")),
    }
}

/// Lets the user pick one partition when there are several. Returns
/// `Ok(None)` for "back", `Ok(Some(None))` for "all".
fn pick_partition(theme: &ColorfulTheme, s: &Session) -> Result<Option<Option<usize>>> {
    if s.partitions.len() <= 1 {
        return Ok(Some(None));
    }
    let mut items = vec!["All partitions (recommended)".to_string()];
    for p in &s.partitions {
        let fs = p.fs.map_or_else(|| "unknown file system".into(), |f| f.to_string());
        let name = if p.name.is_empty() { p.kind.clone() } else { format!("{} \"{}\"", p.kind, p.name) };
        items.push(format!("Partition {}  {:>10}  {fs}  ({name})", p.index, format_size(p.len)));
    }
    items.push("<- Back".into());
    let choice = Select::with_theme(theme).with_prompt("Which partition?").items(&items).default(0).interact_opt()?;
    Ok(match choice {
        Some(0) => Some(None),
        Some(i) if i <= s.partitions.len() => Some(Some(s.partitions[i - 1].index)),
        _ => None,
    })
}

/// What kind of files to look for, as a filter and a short description.
fn pick_types(theme: &ColorfulTheme) -> Result<Option<(Filter, String)>> {
    let items = [
        "Everything",
        "Photos & images",
        "Videos",
        "Music & audio",
        "Documents (PDF, Word, Excel, PowerPoint, ...)",
        "Choose several categories...",
        "Specific file types (e.g. jpg, mp4, docx)...",
        "<- Back",
    ];
    let one = |c: Category, label: &str| -> Result<Option<(Filter, String)>> {
        Ok(Some((Filter::new(&[], &[c], None, 0, None)?, label.to_string())))
    };
    match Select::with_theme(theme).with_prompt("What are you looking for?").items(items).default(0).interact_opt()? {
        Some(0) => Ok(Some((Filter::default(), "Everything".into()))),
        Some(1) => one(Category::Image, "Photos & images"),
        Some(2) => one(Category::Video, "Videos"),
        Some(3) => one(Category::Audio, "Music & audio"),
        Some(4) => one(Category::Document, "Documents"),
        Some(5) => {
            let labels: Vec<String> = Category::ALL.iter().map(|c| capitalize(c.dir_name())).collect();
            let picked = MultiSelect::with_theme(theme)
                .with_prompt("Select with Space, confirm with Enter")
                .items(&labels)
                .interact_opt()?;
            match picked {
                Some(idx) if !idx.is_empty() => {
                    let cats: Vec<Category> = idx.iter().map(|&i| Category::ALL[i]).collect();
                    let label = idx.iter().map(|&i| labels[i].clone()).collect::<Vec<_>>().join(", ");
                    Ok(Some((Filter::new(&[], &cats, None, 0, None)?, label)))
                }
                _ => Ok(None),
            }
        }
        Some(6) => {
            let s: String = Input::with_theme(theme).with_prompt("File types, separated by commas").interact_text()?;
            let exts: Vec<String> =
                s.split([',', ' ', ';']).map(str::trim).filter(|e| !e.is_empty()).map(String::from).collect();
            if exts.is_empty() {
                return Ok(None);
            }
            let label = exts.join(", ");
            Ok(Some((Filter::new(&exts, &[], None, 0, None)?, label)))
        }
        _ => Ok(None),
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn pick_method(theme: &ColorfulTheme) -> Result<Option<(Method, &'static str)>> {
    let items = [
        "Recommended - deleted files with their names, then a deep search for the rest",
        "Quick - only deleted files that still have their names and folders",
        "Formatted or corrupted drive - deep search by file content only",
        "<- Back",
    ];
    Ok(match Select::with_theme(theme).with_prompt("How should I search?").items(items).default(0).interact_opt()? {
        Some(0) => Some((Method::All, "Recommended")),
        Some(1) => Some((Method::Fs, "Quick")),
        Some(2) => Some((Method::Carve, "Deep search by content")),
        _ => None,
    })
}

fn default_output_dir() -> PathBuf {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    let base = home.map(|h| if h.join("Desktop").is_dir() { h.join("Desktop") } else { h }).unwrap_or_default();
    base.join(format!("Recovered Files {}", chrono::Local::now().format("%Y-%m-%d %H%M")))
}

fn pick_output(theme: &ColorfulTheme, source: &str) -> Result<Option<PathBuf>> {
    println!(
        "  {}",
        style("The recovered files must be saved on a DIFFERENT drive than the one you are recovering.").yellow()
    );
    let mut suggestion = default_output_dir();
    loop {
        let typed: String = Input::with_theme(theme)
            .with_prompt("Save recovered files to (leave empty to go back)")
            .with_initial_text(suggestion.to_string_lossy())
            .allow_empty(true)
            .interact_text()?;
        let typed = typed.trim().trim_matches('"');
        if typed.is_empty() {
            return Ok(None);
        }
        let out = PathBuf::from(typed);
        match wdfr::output::ensure_not_on_source(source, &out) {
            Ok(()) => return Ok(Some(out)),
            Err(_) => {
                println!(
                    "  {} That folder is on the drive you are recovering from. Please choose a folder on another drive (for example a USB stick or external disk).",
                    style("Not allowed:").red().bold()
                );
                suggestion = out;
            }
        }
    }
}

/// The full guided recovery. `preset` skips the drive question.
fn recover_wizard(theme: &ColorfulTheme, preset: Option<(String, Option<usize>)>) -> Result<()> {
    header("Recover deleted files");
    let (source, part) = match preset {
        Some(p) => p,
        None => {
            let Some(source) = pick_source(theme)? else { return Ok(()) };
            let session = Session::open(&source)?;
            let Some(part) = pick_partition(theme, &session)? else { return Ok(()) };
            (session.path.clone(), part)
        }
    };
    let Some((mut filter, what)) = pick_types(theme)? else { return Ok(()) };
    let Some((method, how)) = pick_method(theme)? else { return Ok(()) };
    let skip_small = Confirm::with_theme(theme)
        .with_prompt("Skip very small files (under 10 KB, mostly icons and thumbnails)?")
        .default(matches!(what.as_str(), "Photos & images"))
        .interact()?;
    if skip_small {
        filter.min_size = 10 << 10;
    }
    let Some(out) = pick_output(theme, &source)? else { return Ok(()) };

    println!();
    println!("  {:<14}{}", style("Drive:").bold(), source);
    if let Some(p) = part {
        println!("  {:<14}partition {p}", style("Partition:").bold());
    }
    println!("  {:<14}{what}", style("Looking for:").bold());
    println!("  {:<14}{how}", style("Search:").bold());
    println!("  {:<14}{}", style("Save to:").bold(), out.display());
    println!();
    if !Confirm::with_theme(theme).with_prompt("Start the recovery now?").default(true).interact()? {
        return Ok(());
    }
    println!("  {}\n", style("Working... press Ctrl+C to stop early (what was found so far is kept).").dim());
    let opts = Options {
        scan: wdfr::recover::ScanOptions {
            method,
            partition: part,
            filter,
            carve_all_space: false,
            step: 512,
            max_carve_size: None,
        },
        save: wdfr::recover::SaveOptions {
            out: out.clone(),
            layout: wdfr::recover::Layout::Original,
            restore_dates: true,
            write_report: true,
            allow_same_volume: false,
        },
        include_overwritten: false,
        keep_duplicates: false,
    };
    crate::cmd_recover(&source, &opts, false)?;
    println!("\n  {}", style(POWERED_BY).yellow());
    if out.is_dir()
        && Confirm::with_theme(theme)
            .with_prompt("Open the folder with the recovered files?")
            .default(true)
            .interact()?
    {
        open_folder(&out);
    }
    Ok(())
}

fn preview(theme: &ColorfulTheme) -> Result<()> {
    header("Preview deleted files");
    let Some(source) = pick_source(theme)? else { return Ok(()) };
    let session = Session::open(&source)?;
    let Some(part) = pick_partition(theme, &session)? else { return Ok(()) };
    let Some((filter, _)) = pick_types(theme)? else { return Ok(()) };
    crate::cmd_scan(&session.path, part, &filter, false, false)?;
    println!("\n  {}", style("Files that are not listed may still be found by the deep search during recovery.").dim());
    if Confirm::with_theme(theme).with_prompt("Recover files from this drive now?").default(true).interact()? {
        recover_wizard(theme, Some((session.path.clone(), part)))?;
    }
    Ok(())
}

fn drive_info(theme: &ColorfulTheme) -> Result<()> {
    header("Drive information");
    let Some(source) = pick_source(theme)? else { return Ok(()) };
    crate::cmd_info(&source).map(|_| ())
}

fn tips() {
    header("Tips for a successful recovery");
    let tips = [
        "Stop using the drive right away. Every new file can overwrite deleted ones.",
        "Never save recovered files to the same drive you are recovering from.",
        "Run this program from another drive or a USB stick, not from the affected drive.",
        "To scan drives on Windows, right-click wdfr.exe and choose \"Run as administrator\".",
        "Memory cards, USB sticks and hard drives usually recover well.",
        "SSDs often erase deleted files automatically (TRIM); recovery may be impossible.",
        "If the drive makes noises or is very slow, copy it to an image file first (e.g. with ddrescue).",
        "Files marked \"overwritten\" are skipped because their content was replaced by other data.",
    ];
    for (i, t) in tips.iter().enumerate() {
        println!("  {}. {t}", i + 1);
    }
    println!();
}

fn open_folder(path: &Path) {
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(program).arg(path).spawn() {
        println!("  Could not open the folder ({e}). It is at: {}", path.display());
    }
}
