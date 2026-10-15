//! The menu bar: on macOS the real one next to the Apple logo, on Windows
//! and Linux a menu bar at the top of the window. Both give the same
//! commands, which `App::run_command` carries out.

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers, RichText, Ui};

use crate::i18n::tr;
use crate::settings::ThemeChoice;
use crate::theme::Palette;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    About,
    CheckUpdates,
    Settings,
    NewScan,
    OpenImage,
    OpenSavedScan,
    SaveScan,
    ShowResults,
    Theme(ThemeChoice),
    Bigger,
    Smaller,
    ActualSize,
    FullScreen,
    Help,
    ProjectPage,
    ShowLog,
    Quit,
}

/// What the menus can offer right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    pub has_results: bool,
    /// A scan, a save or a drive copy is running.
    pub busy: bool,
    pub theme: ThemeChoice,
}

impl Command {
    fn enabled(self, s: &State) -> bool {
        match self {
            Command::SaveScan | Command::ShowResults => s.has_results,
            Command::NewScan | Command::OpenImage | Command::OpenSavedScan => !s.busy,
            _ => true,
        }
    }
}

const THEMES: [(ThemeChoice, &str); 4] = [
    (ThemeChoice::System, "System"),
    (ThemeChoice::Light, "Light"),
    (ThemeChoice::Dark, "Dark"),
    (ThemeChoice::Midnight, "Midnight"),
];

// ----------------------------------------------------------------------
// Windows and Linux: drawn in the window.

/// Keyboard shortcuts of the window's menu bar (on macOS the menu bar
/// handles them).
const SHORTCUTS: [(Command, KeyboardShortcut); 8] = [
    (Command::NewScan, KeyboardShortcut::new(Modifiers::COMMAND, Key::N)),
    (Command::OpenImage, KeyboardShortcut::new(Modifiers::COMMAND, Key::O)),
    (Command::OpenSavedScan, KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::O)),
    (Command::SaveScan, KeyboardShortcut::new(Modifiers::COMMAND, Key::S)),
    (Command::Settings, KeyboardShortcut::new(Modifiers::COMMAND, Key::Comma)),
    (Command::FullScreen, KeyboardShortcut::new(Modifiers::NONE, Key::F11)),
    (Command::Help, KeyboardShortcut::new(Modifiers::NONE, Key::F1)),
    (Command::Quit, KeyboardShortcut::new(Modifiers::COMMAND, Key::Q)),
];

fn shortcut_text(ctx: &egui::Context, c: Command) -> String {
    match c {
        // These are handled with the other interface size keys.
        Command::Bigger => "Ctrl++".into(),
        Command::Smaller => "Ctrl+-".into(),
        Command::ActualSize => "Ctrl+0".into(),
        _ => SHORTCUTS.iter().find(|(k, _)| *k == c).map(|(_, s)| ctx.format_shortcut(s)).unwrap_or_default(),
    }
}

/// The shortcut pressed this frame, if any.
pub fn shortcut(ctx: &egui::Context, s: &State) -> Option<Command> {
    SHORTCUTS.iter().find(|(c, k)| c.enabled(s) && ctx.input_mut(|i| i.consume_shortcut(k))).map(|(c, _)| *c)
}

/// The menu bar at the top of the window. Returns the command chosen.
pub fn bar(ui: &mut Ui, p: &Palette, s: &State) -> Option<Command> {
    let chosen = std::cell::Cell::new(None);
    let ctx = ui.ctx().clone();
    egui::MenuBar::new().ui(ui, |ui| {
        let item = |ui: &mut Ui, c: Command, label: &str| {
            let button = egui::Button::new(label).shortcut_text(RichText::new(shortcut_text(&ctx, c)).color(p.weak));
            if ui.add_enabled(c.enabled(s), button).clicked() {
                chosen.set(Some(c));
            }
        };
        ui.menu_button(tr("File"), |ui| {
            item(ui, Command::NewScan, tr("New scan"));
            item(ui, Command::OpenImage, tr("Open disk image…"));
            item(ui, Command::OpenSavedScan, tr("Open saved scan…"));
            item(ui, Command::SaveScan, tr("Save scan…"));
            ui.separator();
            item(ui, Command::Quit, tr("Exit"));
        });
        ui.menu_button(tr("View"), |ui| {
            item(ui, Command::ShowResults, tr("Results"));
            ui.separator();
            ui.menu_button(tr("Theme"), |ui| {
                for (t, label) in THEMES {
                    if ui.radio(s.theme == t, tr(label)).clicked() {
                        chosen.set(Some(Command::Theme(t)));
                    }
                }
            });
            item(ui, Command::Bigger, tr("Bigger"));
            item(ui, Command::Smaller, tr("Smaller"));
            item(ui, Command::ActualSize, tr("Actual size"));
            ui.separator();
            item(ui, Command::FullScreen, tr("Full screen"));
        });
        // Settings is one click away, not inside a menu.
        if ui.button(tr("Settings")).clicked() {
            chosen.set(Some(Command::Settings));
        }
        ui.menu_button(tr("Help"), |ui| {
            item(ui, Command::Help, tr("Help"));
            item(ui, Command::CheckUpdates, tr("Check for updates…"));
            item(ui, Command::ProjectPage, tr("Project page"));
            item(ui, Command::ShowLog, tr("Show the log file"));
            ui.separator();
            item(ui, Command::About, tr("About Deleted Files Recovery"));
        });
    });
    chosen.get()
}

// ----------------------------------------------------------------------
// macOS: the menu bar next to the Apple logo.

#[cfg(target_os = "macos")]
pub use mac::NativeMenu;

#[cfg(target_os = "macos")]
mod mac {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::mpsc::{Receiver, channel};

    use muda::accelerator::{Key, KeyAccelerator, Modifiers};
    use muda::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

    use super::{Command, State, THEMES};
    use crate::i18n::{Lang, tr};

    pub struct NativeMenu {
        /// Kept alive: dropping it would empty the menu bar.
        _menu: Menu,
        language: Lang,
        commands: HashMap<String, Command>,
        items: Vec<(MenuItem, Command)>,
        themes: Vec<(CheckMenuItem, super::ThemeChoice)>,
        events: Receiver<String>,
        shown: Option<State>,
    }

    fn id(c: Command) -> String {
        format!("{c:?}")
    }

    impl NativeMenu {
        /// Puts the app's menus into the menu bar. Menu clicks wake the
        /// window up through `ctx`.
        pub fn new(ctx: &eframe::egui::Context, language: Lang) -> Option<Self> {
            let (tx, events) = channel();
            let ctx = ctx.clone();
            MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                let _ = tx.send(e.id.0);
                ctx.request_repaint();
            }));
            Self::build(language, events)
        }

        fn build(language: Lang, events: Receiver<String>) -> Option<Self> {
            let commands = RefCell::new(HashMap::new());
            let items = RefCell::new(Vec::new());
            // Shortcuts by the character typed (⌘+ is "+" on every keyboard
            // layout), not by the key's position.
            let item = |c: Command, label: &str, keys: Option<(Modifiers, &str)>| {
                let i = MenuItem::with_id(id(c), label, true, None);
                if let Some((mods, ch)) = keys
                    && let Err(e) = i.set_key_accelerator(Some(KeyAccelerator::new(mods, Key::Character(ch.into()))))
                {
                    log::warn!("menu shortcut {ch}: {e}");
                }
                commands.borrow_mut().insert(id(c), c);
                items.borrow_mut().push((i.clone(), c));
                i
            };
            let name = "Deleted Files Recovery";
            let app = Submenu::with_items(
                name,
                true,
                &[
                    &item(Command::About, tr("About Deleted Files Recovery"), None),
                    &item(Command::CheckUpdates, tr("Check for updates…"), None),
                    &PredefinedMenuItem::separator(),
                    &item(Command::Settings, tr("Settings…"), Some((Modifiers::META, ","))),
                    &PredefinedMenuItem::separator(),
                    &PredefinedMenuItem::services(Some(tr("Services"))),
                    &PredefinedMenuItem::separator(),
                    &PredefinedMenuItem::hide(Some(tr("Hide Deleted Files Recovery"))),
                    &PredefinedMenuItem::hide_others(Some(tr("Hide others"))),
                    &PredefinedMenuItem::show_all(Some(tr("Show all"))),
                    &PredefinedMenuItem::separator(),
                    // Not the predefined Quit: that ends the app before it
                    // saves its settings.
                    &item(Command::Quit, tr("Quit Deleted Files Recovery"), Some((Modifiers::META, "q"))),
                ],
            )
            .ok()?;
            let file = Submenu::with_items(
                tr("File"),
                true,
                &[
                    &item(Command::NewScan, tr("New scan"), Some((Modifiers::META, "n"))),
                    &item(Command::OpenImage, tr("Open disk image…"), Some((Modifiers::META, "o"))),
                    &item(
                        Command::OpenSavedScan,
                        tr("Open saved scan…"),
                        Some((Modifiers::META.union(Modifiers::SHIFT), "o")),
                    ),
                    &item(Command::SaveScan, tr("Save scan…"), Some((Modifiers::META, "s"))),
                ],
            )
            .ok()?;
            let mut themes = Vec::new();
            let theme = Submenu::new(tr("Theme"), true);
            for (t, label) in THEMES {
                let c = Command::Theme(t);
                let i = CheckMenuItem::with_id(id(c), tr(label), true, false, None);
                commands.borrow_mut().insert(id(c), c);
                theme.append(&i).ok()?;
                themes.push((i, t));
            }
            let view = Submenu::with_items(
                tr("View"),
                true,
                &[
                    &item(Command::ShowResults, tr("Results"), Some((Modifiers::META, "r"))),
                    &PredefinedMenuItem::separator(),
                    &theme,
                    &item(Command::Bigger, tr("Bigger"), Some((Modifiers::META, "+"))),
                    &item(Command::Smaller, tr("Smaller"), Some((Modifiers::META, "-"))),
                    &item(Command::ActualSize, tr("Actual size"), Some((Modifiers::META, "0"))),
                    &PredefinedMenuItem::separator(),
                    &PredefinedMenuItem::fullscreen(Some(tr("Full screen"))),
                ],
            )
            .ok()?;
            let window = Submenu::with_items(
                tr("Window"),
                true,
                &[
                    &PredefinedMenuItem::minimize(Some(tr("Minimize"))),
                    &PredefinedMenuItem::maximize(Some(tr("Zoom"))),
                    &PredefinedMenuItem::separator(),
                    &PredefinedMenuItem::bring_all_to_front(Some(tr("Bring all to front"))),
                ],
            )
            .ok()?;
            let help = Submenu::with_items(
                tr("Help"),
                true,
                &[
                    &item(Command::Help, tr("Deleted Files Recovery help"), None),
                    &item(Command::ProjectPage, tr("Project page"), None),
                    &item(Command::ShowLog, tr("Show the log file"), None),
                ],
            )
            .ok()?;
            let menu = Menu::with_items(&[&app, &file, &view, &window, &help]).ok()?;
            menu.init_for_nsapp();
            window.set_as_windows_menu_for_nsapp();
            help.set_as_help_menu_for_nsapp();
            Some(Self {
                _menu: menu,
                language,
                commands: commands.into_inner(),
                items: items.into_inner(),
                themes,
                events,
                shown: None,
            })
        }

        /// The commands chosen since the last frame. Rebuilds the menus in a
        /// new language, and greys out what cannot be done.
        pub fn update(&mut self, language: Lang, state: State) -> Vec<Command> {
            let chosen: Vec<Command> =
                self.events.try_iter().filter_map(|id| self.commands.get(&id).copied()).collect();
            if language != self.language {
                let (_, empty) = channel();
                let events = std::mem::replace(&mut self.events, empty);
                if let Some(new) = Self::build(language, events) {
                    *self = new;
                }
            }
            if self.shown != Some(state) || !chosen.is_empty() {
                for (i, c) in &self.items {
                    i.set_enabled(c.enabled(&state));
                }
                // A click ticks the item by itself: tick the right one again.
                for (i, t) in &self.themes {
                    i.set_checked(*t == state.theme);
                }
                self.shown = Some(state);
            }
            chosen
        }
    }
}
