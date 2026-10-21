//! Tufan desktop UI (GTK 4). All filesystem work runs in the engine's
//! threads; the UI polls aggregated progress snapshots at 4 Hz.

// No console window alongside the UI on Windows.
#![windows_subsystem = "windows"]

#[cfg(not(windows))]
mod clip;
#[cfg(not(windows))]
mod integrate;
#[cfg(windows)]
mod win;

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use tufan_core::journal::{self, ItemState, Journal};
use tufan_core::{ConflictPolicy, IntegrityMode, JobHandle, JobKind, JobSpec, JobState, Options};

const APP_ID: &str = "org.tufan.Tufan";

// No `window { background: ... }` rule: GTK 4.20 dropped the named theme
// colours, and an unresolved one there leaves every window see-through.
const CSS: &str = "
.brand { font-size: 20px; font-weight: 600; letter-spacing: 0.5px; }
.muted { opacity: 0.65; }
.job { padding: 10px 12px; border-bottom: 1px solid alpha(currentColor, 0.08); }
.job-title { font-weight: 600; }
.summary { padding: 6px 12px; }
.log { font-family: monospace; padding: 1px 8px; }
progressbar trough { min-height: 6px; border-radius: 3px; }
progressbar progress { min-height: 6px; border-radius: 3px; }
";

/// Small per-job progress window shown while a transfer runs.
struct Compact {
    win: gtk4::Window,
    title: String,
    bar: gtk4::ProgressBar,
    rate: gtk4::Label,
    counts: gtk4::Label,
    pause: gtk4::Button,
    cancel: gtk4::Button,
}

struct JobRow {
    handle: Option<Arc<JobHandle>>,
    job_id: i64,
    widget: gtk4::Box,
    title: gtk4::Label,
    status: gtk4::Label,
    bar: gtk4::ProgressBar,
    pause: gtk4::Button,
    cancel: gtk4::Button,
    retry: gtk4::Button,
    compact: Option<Compact>,
    /// The finished state has been shown (once per run of the job).
    finish_seen: bool,
    last: (Instant, u64),
    speed: f64,
}

struct App {
    journal: Arc<Journal>,
    sources: Vec<PathBuf>,
    dest: Option<PathBuf>,
    jobs: Vec<JobRow>,
    tray_status: String,
}

type Shared = Rc<RefCell<App>>;

/// Widgets that job handling needs outside `activate`.
#[derive(Clone)]
struct Ui {
    app: gtk4::Application,
    window: gtk4::ApplicationWindow,
    jobs_box: gtk4::Box,
    integrity: gtk4::DropDown,
    conflict: gtk4::DropDown,
}

thread_local! {
    /// The UI once built, for requests arriving on the command line.
    static UI: RefCell<Option<(Shared, Ui)>> = const { RefCell::new(None) };
}

fn main() -> glib::ExitCode {
    // File manager actions that only record the selection: no UI.
    #[cfg(not(windows))]
    if let Some(code) = file_manager_command() {
        return code;
    }
    // --background (used by the login entry) starts hidden in the tray.
    let mut args: Vec<String> = std::env::args().collect();
    let background = cfg!(windows) && args.iter().any(|a| a == "--background");
    args.retain(|a| a != "--background");
    // GApplication uniqueness needs D-Bus, which Windows lacks.
    #[cfg(windows)]
    if !win::claim_single_instance() {
        return glib::ExitCode::SUCCESS;
    }
    // Command lines are forwarded to a running instance (D-Bus), so a
    // paste from the file manager joins the open window's job list.
    let app = gtk4::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    app.add_main_option(
        "paste",
        glib::Char::from(0),
        glib::OptionFlags::NONE,
        glib::OptionArg::Filename,
        "Paste what Tufan Copy / Tufan Cut recorded into FOLDER",
        Some("FOLDER"),
    );
    let first = Cell::new(true);
    app.connect_command_line(move |app, cl| {
        let paste = cl
            .options_dict()
            .lookup::<PathBuf>("paste")
            .ok()
            .flatten()
            .map(|t| paste_target(&t, cl.cwd().unwrap_or_default()));
        // Pasting needs only the progress window.
        let quiet = paste.is_some() || (background && first.get());
        first.set(false);
        activate(app, !quiet);
        if let Some(dest) = paste {
            paste_into(dest);
        }
        glib::ExitCode::SUCCESS
    });
    app.run_with_args(&args)
}

/// `tufan-gtk --copy|--cut PATH...` and `--remove-integration`.
#[cfg(not(windows))]
fn file_manager_command() -> Option<glib::ExitCode> {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let kind = match args.get(1)?.to_str()? {
        "--copy" => JobKind::Copy,
        "--cut" => JobKind::Move,
        "--remove-integration" => {
            integrate::remove();
            return Some(glib::ExitCode::SUCCESS);
        }
        _ => return None,
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let paths: Vec<PathBuf> = args[2..]
        .iter()
        .map(|a| paste_target_raw(Path::new(a), &cwd))
        .collect();
    Some(match clip::store(kind, &paths, &cwd) {
        Ok(()) => glib::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tufan: cannot record the selection: {e}");
            glib::ExitCode::FAILURE
        }
    })
}

/// A path or file:// URI (file managers pass either), made absolute.
fn paste_target_raw(t: &Path, cwd: &Path) -> PathBuf {
    let s = t.to_string_lossy();
    if s.starts_with("file://") {
        if let Some(p) = gio::File::for_uri(&s).path() {
            return p;
        }
    }
    cwd.join(t)
}

/// The folder to paste into: a selected file stands for its folder.
fn paste_target(t: &Path, cwd: PathBuf) -> PathBuf {
    let p = paste_target_raw(t, &cwd);
    match p.parent() {
        Some(parent) if p.is_file() => parent.to_path_buf(),
        _ => p,
    }
}

/// Starts the transfer recorded by Tufan Copy / Tufan Cut.
fn paste_into(dest: PathBuf) {
    let Some((state, ui)) = UI.with(|u| u.borrow().clone()) else {
        return;
    };
    #[cfg(not(windows))]
    let recorded = clip::load();
    #[cfg(windows)]
    let recorded: Option<(JobKind, Vec<PathBuf>)> = None;
    let Some((kind, sources)) = recorded else {
        ui.window.present();
        alert(
            &ui.window,
            "Nothing to paste. Use \u{201c}Tufan Copy\u{201d} or \u{201c}Tufan Cut\u{201d} in the file manager first.",
        );
        return;
    };
    match start_job(&state, &ui, kind, sources, dest) {
        #[cfg(not(windows))]
        Ok(()) if kind == JobKind::Move => clip::clear(),
        Ok(()) => {}
        Err(e) => {
            ui.window.present();
            alert(&ui.window, &e);
        }
    }
}

/// GTK 4.20 deprecates gtk-application-prefer-dark-theme in favour of
/// gtk-interface-color-scheme; use the new property when the runtime has it.
fn set_dark(s: &gtk4::Settings, dark: bool) {
    let scheme = s
        .find_property("gtk-interface-color-scheme")
        .and_then(|p| glib::EnumClass::with_type(p.value_type()))
        .and_then(|c| c.to_value_by_nick(if dark { "dark" } else { "default" }));
    match scheme {
        Some(v) => s.set_property_from_value("gtk-interface-color-scheme", &v),
        None => s.set_gtk_application_prefer_dark_theme(dark),
    }
}

fn activate(app: &gtk4::Application, show: bool) {
    // Single instance: a second launch re-presents the existing window.
    if let Some(w) = app
        .windows()
        .into_iter()
        .find(|w| w.is::<gtk4::ApplicationWindow>())
    {
        if show {
            w.present();
        }
        return;
    }
    #[cfg(not(windows))]
    if let Err(e) = integrate::install() {
        eprintln!("tufan: file manager integration not installed: {e}");
    }
    let provider = gtk4::CssProvider::new();
    provider.load_from_data(CSS);
    gtk4::style_context_add_provider_for_display(
        &gdk::Display::default().expect("display"),
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let journal = match Journal::open(&journal::default_path()) {
        Ok(j) => Arc::new(j),
        Err(e) => {
            eprintln!("tufan: cannot open journal: {e}");
            Arc::new(Journal::open_in_memory().expect("in-memory journal"))
        }
    };
    let state: Shared = Rc::new(RefCell::new(App {
        journal,
        sources: Vec::new(),
        dest: None,
        jobs: Vec::new(),
        tray_status: String::new(),
    }));

    let window = gtk4::ApplicationWindow::builder()
        .application(app)
        .title("Tufan")
        .default_width(720)
        .default_height(560)
        .build();

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    root.set_margin_top(16);
    root.set_margin_bottom(16);
    root.set_margin_start(16);
    root.set_margin_end(16);

    // Branding row.
    let brand_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let brand = gtk4::Label::new(Some("Tufan"));
    brand.add_css_class("brand");
    brand.set_hexpand(true);
    brand.set_xalign(0.0);
    let theme = gtk4::ToggleButton::with_label("Dark");
    theme.set_tooltip_text(Some("Toggle dark theme"));
    theme.connect_toggled(|b| {
        if let Some(s) = gtk4::Settings::default() {
            set_dark(&s, b.is_active());
        }
    });
    brand_row.append(&brand);
    brand_row.append(&theme);
    root.append(&brand_row);

    // Sources.
    let src_label = gtk4::Label::new(Some("No sources selected"));
    src_label.set_xalign(0.0);
    src_label.set_hexpand(true);
    src_label.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    src_label.add_css_class("muted");
    let add_files = gtk4::Button::with_label("Add Files…");
    let add_folder = gtk4::Button::with_label("Add Folder…");
    let paste = gtk4::Button::with_label("From Clipboard");
    paste.set_tooltip_text(Some(
        "Import files copied in the file manager (Ctrl+V). You can also drop files here.",
    ));
    let clear = gtk4::Button::with_label("Clear");
    let src_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    for w in [&add_files, &add_folder, &paste, &clear] {
        src_row.append(w);
    }
    root.append(&section("Source", &src_label));
    root.append(&src_row);

    // Destination.
    let dest_label = gtk4::Label::new(Some("No destination selected"));
    dest_label.set_xalign(0.0);
    dest_label.set_hexpand(true);
    dest_label.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    dest_label.add_css_class("muted");
    let choose_dest = gtk4::Button::with_label("Choose…");
    let dest_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    dest_row.append(&dest_label);
    dest_row.append(&choose_dest);
    root.append(&section_box("Destination", &dest_row));

    // Options + actions.
    let integrity = gtk4::DropDown::from_strings(&["Verified", "Durable verified", "Fast"]);
    integrity.set_tooltip_text(Some(
        "Verified: SHA-256 check before publishing.\nDurable: also flush to disk.\nFast: size and change checks only.",
    ));
    let conflict = gtk4::DropDown::from_strings(&["Ask: fail", "Skip", "Keep both", "Overwrite"]);
    conflict.set_tooltip_text(Some("What to do when a destination file already exists"));
    let copy_btn = gtk4::Button::with_label("Copy");
    copy_btn.add_css_class("suggested-action");
    let move_btn = gtk4::Button::with_label("Move");
    let actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    for w in [
        integrity.upcast_ref::<gtk4::Widget>(),
        conflict.upcast_ref(),
        spacer.upcast_ref(),
        move_btn.upcast_ref(),
        copy_btn.upcast_ref(),
    ] {
        actions.append(w);
    }
    root.append(&actions);

    // Jobs.
    let jobs_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let scroller = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .child(&jobs_box)
        .build();
    let frame = gtk4::Frame::new(None);
    frame.set_child(Some(&scroller));
    let summary = gtk4::Label::new(Some("No active jobs"));
    summary.add_css_class("summary");
    summary.add_css_class("muted");
    summary.set_xalign(0.0);
    root.append(&frame);
    root.append(&summary);

    window.set_child(Some(&root));

    let ui = Ui {
        app: app.clone(),
        window: window.clone(),
        jobs_box: jobs_box.clone(),
        integrity: integrity.clone(),
        conflict: conflict.clone(),
    };

    // --- Behaviour -------------------------------------------------------
    let refresh_src = {
        let (state, label) = (state.clone(), src_label.clone());
        Rc::new(move || {
            let st = state.borrow();
            label.set_text(&match st.sources.len() {
                0 => "No sources selected".to_string(),
                1 => st.sources[0].display().to_string(),
                n => format!("{} and {} more", st.sources[0].display(), n - 1),
            });
        })
    };

    {
        let (state, win, refresh) = (state.clone(), window.clone(), refresh_src.clone());
        add_files.connect_clicked(move |_| {
            let (state, win, refresh) = (state.clone(), win.clone(), refresh.clone());
            glib::spawn_future_local(async move {
                let d = gtk4::FileDialog::builder()
                    .title("Add files")
                    .modal(true)
                    .build();
                if let Ok(list) = d.open_multiple_future(Some(&win)).await {
                    add_from_model(&state, &list);
                    refresh();
                }
            });
        });
    }
    {
        let (state, win, refresh) = (state.clone(), window.clone(), refresh_src.clone());
        add_folder.connect_clicked(move |_| {
            let (state, win, refresh) = (state.clone(), win.clone(), refresh.clone());
            glib::spawn_future_local(async move {
                let d = gtk4::FileDialog::builder()
                    .title("Add folders")
                    .modal(true)
                    .build();
                if let Ok(list) = d.select_multiple_folders_future(Some(&win)).await {
                    add_from_model(&state, &list);
                    refresh();
                }
            });
        });
    }
    {
        let (state, refresh, win) = (state.clone(), refresh_src.clone(), window.clone());
        paste.connect_clicked(move |_| {
            let (state, refresh) = (state.clone(), refresh.clone());
            // Read-only access: the user's clipboard is never modified.
            let clipboard = win.clipboard();
            glib::spawn_future_local(async move {
                if let Ok(v) = clipboard
                    .read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT)
                    .await
                {
                    if let Ok(list) = v.get::<gdk::FileList>() {
                        add_files_list(&state, &list);
                    }
                }
                refresh();
            });
        });
    }
    // Ctrl+V in the window imports the clipboard like the button does.
    {
        let paste = paste.clone();
        let keys = gtk4::ShortcutController::new();
        keys.set_scope(gtk4::ShortcutScope::Managed);
        keys.add_shortcut(gtk4::Shortcut::new(
            gtk4::ShortcutTrigger::parse_string("<Control>v"),
            Some(gtk4::CallbackAction::new(move |_, _| {
                paste.emit_clicked();
                glib::Propagation::Stop
            })),
        ));
        window.add_controller(keys);
    }
    // Files dropped from the file manager become sources.
    {
        let (state, refresh) = (state.clone(), refresh_src.clone());
        let drop = gtk4::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        drop.connect_drop(move |_, v, _, _| {
            let Ok(list) = v.get::<gdk::FileList>() else {
                return false;
            };
            add_files_list(&state, &list);
            refresh();
            true
        });
        window.add_controller(drop);
    }
    {
        let (state, refresh) = (state.clone(), refresh_src.clone());
        clear.connect_clicked(move |_| {
            state.borrow_mut().sources.clear();
            refresh();
        });
    }
    {
        let (state, win, label) = (state.clone(), window.clone(), dest_label.clone());
        choose_dest.connect_clicked(move |_| {
            let (state, win, label) = (state.clone(), win.clone(), label.clone());
            glib::spawn_future_local(async move {
                let d = gtk4::FileDialog::builder()
                    .title("Choose destination")
                    .modal(true)
                    .build();
                if let Ok(f) = d.select_folder_future(Some(&win)).await {
                    if let Some(p) = f.path() {
                        label.set_text(&p.display().to_string());
                        state.borrow_mut().dest = Some(p);
                    }
                }
            });
        });
    }

    for (btn, kind) in [(&copy_btn, JobKind::Copy), (&move_btn, JobKind::Move)] {
        let (state, ui, refresh) = (state.clone(), ui.clone(), refresh_src.clone());
        btn.connect_clicked(move |_| {
            let (sources, dest) = {
                let st = state.borrow();
                (st.sources.clone(), st.dest.clone())
            };
            let Some(dest) = dest else {
                alert(&ui.window, "Choose a destination folder first.");
                return;
            };
            if sources.is_empty() {
                alert(&ui.window, "Add at least one file or folder to transfer.");
                return;
            }
            match start_job(&state, &ui, kind, sources, dest) {
                Ok(()) => {
                    state.borrow_mut().sources.clear();
                    refresh();
                    // The compact window takes over; the main window waits
                    // in the tray.
                    #[cfg(windows)]
                    if win::close_to_tray() {
                        ui.window.set_visible(false);
                    }
                }
                Err(e) => alert(&ui.window, &e),
            }
        });
    }

    // Unfinished jobs from a previous session are offered for resumption,
    // not restarted automatically.
    let unfinished: Vec<_> = state
        .borrow()
        .journal
        .list_jobs()
        .unwrap_or_default()
        .into_iter()
        .filter(|j| !j.state.is_finished())
        .collect();
    for j in unfinished {
        add_job_row(&state, &ui, None, Some(j));
    }

    {
        let (state, summary, app) = (state.clone(), summary.clone(), app.clone());
        glib::timeout_add_local(Duration::from_millis(250), move || {
            tick(&state, &summary);
            // Without a tray, nothing is left to show once every window is
            // closed and no job runs (e.g. a paste from the file manager).
            #[cfg(not(windows))]
            if !any_active(&state)
                && gtk4::Window::list_toplevels()
                    .iter()
                    .all(|w| !w.is_visible())
            {
                app.quit();
            }
            #[cfg(windows)]
            let _ = &app;
            glib::ControlFlow::Continue
        });
    }

    #[cfg(windows)]
    {
        let (state, ui) = (state.clone(), ui.clone());
        win::init(Box::new(move |ev| match ev {
            win::Event::Show => ui.window.present(),
            win::Event::PauseAll => set_all_paused(&state, true),
            win::Event::ResumeAll => set_all_paused(&state, false),
            win::Event::Exit => exit_app(&state, &ui),
            win::Event::Paste {
                kind,
                sources,
                dest,
            } => {
                if let Err(e) = start_job(&state, &ui, kind, sources, dest) {
                    ui.window.present();
                    alert(&ui.window, &e);
                }
            }
        }));
        // The tray keeps Tufan running with no window open; Exit quits.
        std::mem::forget(app.hold());
        app.connect_shutdown(|_| win::remove_tray());
    }

    {
        let (state, ui) = (state.clone(), ui.clone());
        window.connect_close_request(move |w| {
            #[cfg(windows)]
            return close_main(&state, &ui, w);
            #[cfg(not(windows))]
            return keep_running_while_busy(&state, &ui, w);
        });
    }

    UI.with(|u| *u.borrow_mut() = Some((state.clone(), ui.clone())));
    if show {
        window.present();
    }
}

#[cfg(windows)]
fn close_main(state: &Shared, ui: &Ui, w: &gtk4::ApplicationWindow) -> glib::Propagation {
    if win::close_to_tray() {
        w.set_visible(false);
    } else {
        exit_app(state, ui);
    }
    glib::Propagation::Stop
}

/// Keep running while jobs are active even if the window is closed.
#[cfg(not(windows))]
fn keep_running_while_busy(
    state: &Shared,
    ui: &Ui,
    w: &gtk4::ApplicationWindow,
) -> glib::Propagation {
    if !any_active(state) {
        return glib::Propagation::Proceed;
    }
    w.set_visible(false);
    let guard = ui.app.hold();
    let state = state.clone();
    glib::timeout_add_local(Duration::from_millis(500), move || {
        if any_active(&state) {
            let _ = &guard;
            glib::ControlFlow::Continue
        } else {
            glib::ControlFlow::Break
        }
    });
    glib::Propagation::Stop
}

fn any_active(state: &Shared) -> bool {
    state
        .borrow()
        .jobs
        .iter()
        .any(|r| r.handle.as_ref().is_some_and(|h| !h.is_finished()))
}

#[cfg_attr(not(windows), allow(dead_code))]
fn set_all_paused(state: &Shared, paused: bool) {
    for r in &state.borrow().jobs {
        if let Some(h) = r.handle.as_ref().filter(|h| !h.is_finished()) {
            if paused {
                h.pause();
            } else {
                h.resume();
            }
        }
    }
}

/// Quit from the tray. Interrupted jobs stay in the journal and are
/// offered for resumption on the next start.
#[cfg_attr(not(windows), allow(dead_code))]
fn exit_app(state: &Shared, ui: &Ui) {
    if !any_active(state) {
        ui.app.quit();
        return;
    }
    ui.window.present();
    let d = gtk4::AlertDialog::builder()
        .message("Transfers are still running")
        .detail("Exiting pauses them. They can be resumed the next time Tufan starts.")
        .buttons(["Keep Running", "Pause and Exit"])
        .cancel_button(0)
        .default_button(0)
        .modal(true)
        .build();
    let (state, ui) = (state.clone(), ui.clone());
    glib::spawn_future_local(async move {
        if d.choose_future(Some(&ui.window)).await == Ok(1) {
            set_all_paused(&state, true);
            ui.app.quit();
        }
    });
}

/// Starts a transfer with the options selected in the main window.
fn start_job(
    state: &Shared,
    ui: &Ui,
    kind: JobKind,
    mut sources: Vec<PathBuf>,
    dest: PathBuf,
) -> Result<(), String> {
    let in_place = |p: &PathBuf| p.parent() == Some(dest.as_path());
    let mut conflict = [
        ConflictPolicy::Fail,
        ConflictPolicy::Skip,
        ConflictPolicy::Rename,
        ConflictPolicy::Overwrite,
    ][ui.conflict.selected() as usize % 4];
    match kind {
        // Moving an item into its own folder is a no-op, as in Explorer.
        JobKind::Move => sources.retain(|p| !in_place(p)),
        // Copying into the same folder keeps both, as in Explorer.
        JobKind::Copy if sources.iter().any(in_place) => conflict = ConflictPolicy::Rename,
        JobKind::Copy => {}
    }
    if sources.is_empty() {
        return Ok(());
    }
    let options = Options {
        integrity: [
            IntegrityMode::Verified,
            IntegrityMode::DurableVerified,
            IntegrityMode::Fast,
        ][ui.integrity.selected() as usize % 3],
        conflict,
        ..Options::default()
    };
    let journal = state.borrow().journal.clone();
    let h = tufan_core::start(
        journal,
        JobSpec {
            kind,
            sources,
            dest_dir: dest,
            options,
        },
    )
    .map_err(|e| e.to_string())?;
    add_job_row(state, ui, Some(Arc::new(h)), None);
    Ok(())
}

fn section(title: &str, body: &gtk4::Label) -> gtk4::Box {
    let b = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    b.append(body);
    section_box(title, &b)
}

fn section_box(title: &str, body: &gtk4::Box) -> gtk4::Box {
    let v = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    let l = gtk4::Label::new(Some(title));
    l.set_xalign(0.0);
    l.add_css_class("heading");
    v.append(&l);
    v.append(body);
    v
}

fn add_from_model(state: &Shared, list: &gio::ListModel) {
    let mut st = state.borrow_mut();
    for i in 0..list.n_items() {
        if let Some(p) = list
            .item(i)
            .and_downcast::<gio::File>()
            .and_then(|f| f.path())
        {
            if !st.sources.contains(&p) {
                st.sources.push(p);
            }
        }
    }
}

fn add_files_list(state: &Shared, list: &gdk::FileList) {
    let mut st = state.borrow_mut();
    for f in list.files() {
        if let Some(p) = f.path() {
            if !st.sources.contains(&p) {
                st.sources.push(p);
            }
        }
    }
}

fn alert(win: &gtk4::ApplicationWindow, msg: &str) {
    let d = gtk4::AlertDialog::builder()
        .message(msg)
        .modal(true)
        .build();
    d.show(Some(win));
}

fn job_description(spec: &JobSpec) -> (&'static str, String) {
    let verb = if spec.kind == JobKind::Copy {
        "Copy"
    } else {
        "Move"
    };
    let what = match spec.sources.len() {
        1 => spec.sources[0]
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        n => format!("{n} items"),
    };
    (verb, what)
}

fn add_job_row(
    state: &Shared,
    ui: &Ui,
    handle: Option<Arc<JobHandle>>,
    restored: Option<tufan_core::journal::JobRow>,
) {
    let (job_id, spec) = match (&handle, &restored) {
        (Some(h), _) => (h.id(), h.spec().clone()),
        (None, Some(r)) => (r.id, r.spec.clone()),
        _ => return,
    };
    let widget = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    widget.add_css_class("job");
    let (verb, what) = job_description(&spec);
    let title = gtk4::Label::new(Some(&format!(
        "{verb} {what} → {}",
        spec.dest_dir.display()
    )));
    title.add_css_class("job-title");
    title.set_xalign(0.0);
    title.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    let bar = gtk4::ProgressBar::new();
    let status = gtk4::Label::new(Some(if handle.is_some() {
        "Scanning…"
    } else {
        "Interrupted — ready to resume"
    }));
    status.add_css_class("muted");
    status.set_xalign(0.0);
    status.set_hexpand(true);
    let log = gtk4::Button::with_label("Log");
    log.set_tooltip_text(Some("Show the per-file log"));
    let pause = gtk4::Button::with_label("Pause");
    let cancel = gtk4::Button::with_label("Cancel");
    let retry = gtk4::Button::with_label(if handle.is_some() {
        "Retry Failed"
    } else {
        "Resume"
    });
    retry.set_visible(handle.is_none());
    pause.set_visible(handle.is_some());
    cancel.set_visible(handle.is_some());
    let controls = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    for w in [
        status.upcast_ref::<gtk4::Widget>(),
        log.upcast_ref(),
        pause.upcast_ref(),
        cancel.upcast_ref(),
        retry.upcast_ref(),
    ] {
        controls.append(w);
    }
    widget.append(&title);
    widget.append(&bar);
    widget.append(&controls);
    ui.jobs_box.append(&widget);

    let idx = state.borrow().jobs.len();
    {
        let (state, ui) = (state.clone(), ui.clone());
        log.connect_clicked(move |_| open_log(&state, &ui, idx));
    }
    {
        let state = state.clone();
        pause.connect_clicked(move |_| toggle_pause(&state, idx));
    }
    {
        let state = state.clone();
        cancel.connect_clicked(move |_| cancel_job(&state, idx));
    }
    {
        let (state, ui) = (state.clone(), ui.clone());
        retry.connect_clicked(move |_| {
            let mut st = state.borrow_mut();
            let journal = st.journal.clone();
            let row = &mut st.jobs[idx];
            match tufan_core::resume(journal, row.job_id) {
                Ok(h) => {
                    let h = Arc::new(h);
                    row.handle = Some(h.clone());
                    row.finish_seen = false;
                    row.retry.set_visible(false);
                    row.pause.set_visible(true);
                    row.pause.set_label("Pause");
                    row.cancel.set_visible(true);
                    if let Some(c) = row.compact.take() {
                        c.win.destroy();
                    }
                    drop(st);
                    let c = compact_window(&state, &ui, idx, h.spec());
                    state.borrow_mut().jobs[idx].compact = Some(c);
                }
                Err(e) => row.status.set_text(&format!("Cannot resume: {e}")),
            }
        });
    }

    let compact = handle
        .as_ref()
        .map(|h| compact_window(state, ui, idx, h.spec()));
    state.borrow_mut().jobs.push(JobRow {
        handle,
        job_id,
        widget,
        title,
        status,
        bar,
        pause,
        cancel,
        retry,
        compact,
        finish_seen: false,
        last: (Instant::now(), 0),
        speed: 0.0,
    });
}

fn toggle_pause(state: &Shared, idx: usize) {
    if let Some(h) = &state.borrow().jobs[idx].handle {
        if h.state() == JobState::Paused {
            h.resume();
        } else {
            h.pause();
        }
    }
}

fn cancel_job(state: &Shared, idx: usize) {
    if let Some(h) = &state.borrow().jobs[idx].handle {
        h.cancel();
    }
}

fn compact_window(state: &Shared, ui: &Ui, idx: usize, spec: &JobSpec) -> Compact {
    let (verb, what) = job_description(spec);
    let verb = if verb == "Copy" { "Copying" } else { "Moving" };
    let base = format!("{verb} {what}");
    let win = gtk4::Window::builder()
        .title(&base)
        .default_width(460)
        .resizable(false)
        .hide_on_close(true)
        .build();
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    root.set_margin_top(14);
    root.set_margin_bottom(14);
    root.set_margin_start(16);
    root.set_margin_end(16);
    let title = gtk4::Label::new(Some(&base));
    title.add_css_class("job-title");
    title.set_xalign(0.0);
    title.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    let to = gtk4::Label::new(Some(&format!("to {}", spec.dest_dir.display())));
    to.add_css_class("muted");
    to.set_xalign(0.0);
    to.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    let bar = gtk4::ProgressBar::new();
    bar.set_margin_top(4);
    let rate = gtk4::Label::new(Some("Scanning…"));
    rate.set_xalign(0.0);
    let counts = gtk4::Label::new(None);
    counts.add_css_class("muted");
    counts.set_xalign(0.0);
    let log = gtk4::Button::with_label("Log");
    let details = gtk4::Button::with_label("Details");
    details.set_tooltip_text(Some("Open the main Tufan window"));
    let pause = gtk4::Button::with_label("Pause");
    let cancel = gtk4::Button::with_label("Cancel");
    let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    buttons.set_margin_top(6);
    let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    for w in [
        log.upcast_ref::<gtk4::Widget>(),
        details.upcast_ref(),
        spacer.upcast_ref(),
        pause.upcast_ref(),
        cancel.upcast_ref(),
    ] {
        buttons.append(w);
    }
    for w in [
        title.upcast_ref::<gtk4::Widget>(),
        to.upcast_ref(),
        bar.upcast_ref(),
        rate.upcast_ref(),
        counts.upcast_ref(),
        buttons.upcast_ref(),
    ] {
        root.append(w);
    }
    win.set_child(Some(&root));
    {
        let (state, ui) = (state.clone(), ui.clone());
        log.connect_clicked(move |_| open_log(&state, &ui, idx));
    }
    {
        let main = ui.window.clone();
        details.connect_clicked(move |_| main.present());
    }
    {
        let state = state.clone();
        pause.connect_clicked(move |_| toggle_pause(&state, idx));
    }
    {
        let state = state.clone();
        cancel.connect_clicked(move |_| cancel_job(&state, idx));
    }
    // On top while the transfer runs, at the place the user last left a
    // progress window (GTK 4 offers neither, so Windows only).
    #[cfg(windows)]
    {
        let offset = 32 * open_compacts(state) as i32;
        win.connect_map(move |w| {
            let w = w.clone();
            glib::idle_add_local_once(move || {
                win::restore_position(&w, offset);
                win::set_topmost(&w, true);
            });
        });
        win.connect_close_request(|w| {
            win::save_position(w);
            glib::Propagation::Proceed
        });
    }
    win.present();
    Compact {
        win,
        title: base,
        bar,
        rate,
        counts,
        pause,
        cancel,
    }
}

/// Progress windows currently on screen, for cascading new ones.
#[cfg(windows)]
fn open_compacts(state: &Shared) -> usize {
    state
        .borrow()
        .jobs
        .iter()
        .filter(|r| r.compact.as_ref().is_some_and(|c| c.win.is_visible()))
        .count()
}

// --- Job log ------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Level {
    Success,
    Skipped,
    Failed,
    Error,
    Pending,
}

const LOG_FILTERS: [&str; 5] = ["All", "Success", "Skipped", "Failed", "Errors"];

fn level_shown(filter: u32, l: Level) -> bool {
    match filter {
        1 => l == Level::Success,
        2 => l == Level::Skipped,
        3 => l == Level::Failed,
        4 => l == Level::Error,
        _ => true,
    }
}

fn log_entries(journal: &Journal, job_id: i64, handle: Option<&JobHandle>) -> Vec<(Level, String)> {
    let mut out = Vec::new();
    let mut file_errors = HashSet::new();
    let mut files = Vec::new();
    let roots: Vec<PathBuf> = journal
        .load_job(job_id)
        .ok()
        .flatten()
        .map(|j| j.spec.sources)
        .unwrap_or_default();
    // Paths as seen from the folder the sources were copied from.
    let relative = |p: &Path| {
        roots
            .iter()
            .find(|r| p.starts_with(r))
            .and_then(|r| r.parent())
            .and_then(|base| p.strip_prefix(base).ok())
            .unwrap_or(p)
            .display()
            .to_string()
    };
    for r in journal.items(job_id).unwrap_or_default() {
        let path = journal::key_path(&r.key);
        let src = path
            .as_deref()
            .map(relative)
            .unwrap_or_else(|| String::from_utf8_lossy(&r.key).into_owned());
        let entry = match (r.state, r.error) {
            (ItemState::Failed | ItemState::Partial | ItemState::Committed, Some(e)) => {
                let line = format!("FAILED   {src}: {e}");
                file_errors.insert(e);
                (Level::Failed, line)
            }
            (ItemState::Failed, None) => (Level::Failed, format!("FAILED   {src}")),
            (ItemState::Done, _) => {
                // Only worth showing when "Keep both" renamed the copy.
                let dest = r
                    .dest
                    .filter(|d| d.file_name() != path.as_deref().and_then(|p| p.file_name()))
                    .and_then(|d| d.file_name().map(|n| format!(" → {}", n.to_string_lossy())));
                (
                    Level::Success,
                    format!(
                        "OK       {src}{}  ({})",
                        dest.unwrap_or_default(),
                        fmt_bytes(r.size)
                    ),
                )
            }
            (ItemState::Skipped, _) => (Level::Skipped, format!("SKIPPED  {src}")),
            (ItemState::Partial | ItemState::Committed, None) => {
                (Level::Pending, format!("PENDING  {src}"))
            }
        };
        files.push(entry);
    }
    // Job-level errors (scan failures and the like) that no file row shows.
    if let Some(h) = handle {
        let report = h.report();
        for e in report.errors {
            if !file_errors.contains(&e) {
                out.push((Level::Error, format!("ERROR    {e}")));
            }
        }
        if report.errors_truncated {
            out.push((
                Level::Error,
                "ERROR    (further errors not recorded)".into(),
            ));
        }
    }
    out.extend(files);
    out
}

fn open_log(state: &Shared, ui: &Ui, idx: usize) {
    let (journal, job_id, handle, title) = {
        let st = state.borrow();
        let r = &st.jobs[idx];
        (
            st.journal.clone(),
            r.job_id,
            r.handle.clone(),
            r.title.text().to_string(),
        )
    };
    let load = move || log_entries(&journal, job_id, handle.as_deref());
    let entries = Rc::new(RefCell::new(load()));

    let win = gtk4::Window::builder()
        .title(format!("Log — {title}"))
        .default_width(820)
        .default_height(480)
        .transient_for(&ui.window)
        .build();
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    root.set_margin_top(12);
    root.set_margin_bottom(12);
    root.set_margin_start(12);
    root.set_margin_end(12);
    let filter = gtk4::DropDown::from_strings(&LOG_FILTERS);
    filter.set_tooltip_text(Some("Show only entries of this kind"));
    let counts = gtk4::Label::new(None);
    counts.add_css_class("muted");
    counts.set_xalign(0.0);
    counts.set_hexpand(true);
    let refresh = gtk4::Button::with_label("Refresh");
    let save = gtk4::Button::with_label("Save…");
    let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    for w in [
        filter.upcast_ref::<gtk4::Widget>(),
        counts.upcast_ref(),
        refresh.upcast_ref(),
        save.upcast_ref(),
    ] {
        bar.append(w);
    }

    let model = gtk4::StringList::new(&[]);
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let l = gtk4::Label::new(None);
        l.set_xalign(0.0);
        l.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        l.add_css_class("log");
        if let Some(item) = item.downcast_ref::<gtk4::ListItem>() {
            item.set_child(Some(&l));
        }
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk4::ListItem>() else {
            return;
        };
        let text = item
            .item()
            .and_downcast::<gtk4::StringObject>()
            .map(|s| s.string());
        if let (Some(l), Some(text)) = (item.child().and_downcast::<gtk4::Label>(), text) {
            l.set_text(&text);
            l.set_tooltip_text(Some(&text));
        }
    });
    let list = gtk4::ListView::new(
        Some(gtk4::NoSelection::new(Some(model.clone()))),
        Some(factory),
    );
    let scroller = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .child(&list)
        .build();
    let frame = gtk4::Frame::new(None);
    frame.set_child(Some(&scroller));
    root.append(&bar);
    root.append(&frame);
    win.set_child(Some(&root));

    let apply = {
        let (entries, filter, model, counts) = (
            entries.clone(),
            filter.clone(),
            model.clone(),
            counts.clone(),
        );
        Rc::new(move || {
            let entries = entries.borrow();
            let f = filter.selected();
            let shown: Vec<&str> = entries
                .iter()
                .filter(|(l, _)| level_shown(f, *l))
                .map(|(_, s)| s.as_str())
                .collect();
            model.splice(0, model.n_items(), &shown);
            let n = |level| entries.iter().filter(|(l, _)| *l == level).count();
            counts.set_text(&format!(
                "{} succeeded · {} skipped · {} failed · {} errors",
                n(Level::Success),
                n(Level::Skipped),
                n(Level::Failed),
                n(Level::Error)
            ));
        })
    };
    apply();
    {
        let apply = apply.clone();
        filter.connect_selected_notify(move |_| apply());
    }
    {
        let (entries, apply) = (entries.clone(), apply.clone());
        refresh.connect_clicked(move |_| {
            *entries.borrow_mut() = load();
            apply();
        });
    }
    {
        let (entries, filter, win2) = (entries.clone(), filter.clone(), win.clone());
        save.connect_clicked(move |_| {
            let f = filter.selected();
            let text: String = entries
                .borrow()
                .iter()
                .filter(|(l, _)| level_shown(f, *l))
                .map(|(_, s)| format!("{s}\n"))
                .collect();
            let win = win2.clone();
            glib::spawn_future_local(async move {
                let d = gtk4::FileDialog::builder()
                    .title("Save log")
                    .initial_name(format!("tufan-job-{job_id}.log"))
                    .modal(true)
                    .build();
                if let Ok(file) = d.save_future(Some(&win)).await {
                    if let Some(p) = file.path() {
                        if let Err(e) = std::fs::write(&p, text) {
                            let a = gtk4::AlertDialog::builder()
                                .message(format!("Cannot save the log: {e}"))
                                .modal(true)
                                .build();
                            a.show(Some(&win));
                        }
                    }
                }
            });
        });
    }
    win.present();
}

// --- Progress -------------------------------------------------------------------

fn tick(state: &Shared, summary: &gtk4::Label) {
    let mut st = state.borrow_mut();
    let (mut active, mut bytes, mut speed_total) = (0, 0u64, 0f64);
    let (mut active_done, mut active_known) = (0u64, 0u64);
    for row in st.jobs.iter_mut() {
        let Some(h) = row.handle.clone() else {
            continue;
        };
        let s = h.snapshot();
        let dt = row.last.0.elapsed().as_secs_f64();
        if dt > 0.0 {
            let inst = s.bytes_done.saturating_sub(row.last.1) as f64 / dt;
            row.speed = if row.speed == 0.0 {
                inst
            } else {
                row.speed * 0.8 + inst * 0.2
            };
        }
        row.last = (Instant::now(), s.bytes_done);
        let finished = h.is_finished();
        let state_now = if finished {
            h.state()
        } else if h.state() == JobState::Paused {
            JobState::Paused
        } else {
            JobState::Running
        };

        // Honest progress: pulse while totals are unknown.
        let fraction = if s.scan_complete && s.bytes_known > 0 {
            Some((s.bytes_done as f64 / s.bytes_known as f64).min(1.0))
        } else if s.scan_complete {
            Some(if finished { 1.0 } else { 0.0 })
        } else {
            None
        };
        let total = if s.scan_complete {
            s.files_discovered.to_string()
        } else {
            format!("{}+", s.files_discovered)
        };
        let files = format!("{}/{} files", s.files_done + s.files_skipped, total);
        let of_bytes = format!(
            "{} of {}",
            fmt_bytes(s.bytes_done),
            if s.scan_complete {
                fmt_bytes(s.bytes_known)
            } else {
                "…".into()
            }
        );
        let mut rate = String::new();
        if !finished && state_now == JobState::Running {
            rate = format!("{}/s", fmt_bytes(row.speed as u64));
            if s.scan_complete && row.speed > 1024.0 && s.bytes_known > s.bytes_done {
                let secs = (s.bytes_known - s.bytes_done) as f64 / row.speed;
                if secs > 3.0 {
                    rate += &format!(" · about {} left", fmt_dur(secs));
                }
            }
            active += 1;
            speed_total += row.speed;
            active_done += s.bytes_done;
            active_known += s.bytes_known.max(s.bytes_done);
        }
        let failed = if s.files_failed > 0 {
            format!(" · {} failed", s.files_failed)
        } else {
            String::new()
        };
        let label = label_for(state_now, s.scan_complete);
        let mut text = format!("{label} · {files} · {of_bytes}");
        if !rate.is_empty() {
            text += &format!(" · {rate}");
        }
        text += &failed;
        if s.files_failed > 0 {
            if let Some(e) = h.report().errors.first() {
                row.status.set_tooltip_text(Some(e));
            }
        }
        row.status.set_text(&text);
        let paused = state_now == JobState::Paused;
        row.pause.set_label(if paused { "Resume" } else { "Pause" });
        match fraction {
            Some(f) => row.bar.set_fraction(f),
            None if !finished => row.bar.pulse(),
            None => {}
        }

        if let Some(c) = &row.compact {
            match fraction {
                Some(f) => c.bar.set_fraction(f),
                None if !finished => c.bar.pulse(),
                None => {}
            }
            let pct = fraction.map(|f| format!("{:.0}% · ", f * 100.0));
            c.rate.set_text(&if finished || paused {
                format!("{}{label}", pct.clone().unwrap_or_default())
            } else if rate.is_empty() {
                label.to_string()
            } else {
                format!("{}{rate}", pct.clone().unwrap_or_default())
            });
            c.counts.set_text(&format!("{files} · {of_bytes}{failed}"));
            c.pause.set_label(if paused { "Resume" } else { "Pause" });
            if let Some(p) = &pct {
                let title = format!("{} — {}", p.trim_end_matches(" · "), c.title);
                c.win.set_title(Some(&title));
            }
        }

        bytes += s.bytes_done;
        if finished && !row.finish_seen {
            row.finish_seen = true;
            row.pause.set_visible(false);
            row.cancel.set_visible(false);
            row.retry.set_label("Retry Failed");
            row.retry.set_visible(
                s.files_failed > 0 || matches!(state_now, JobState::Cancelled | JobState::Failed),
            );
            if let Some(c) = &row.compact {
                c.pause.set_visible(false);
                c.cancel.set_visible(false);
                #[cfg(windows)]
                {
                    win::set_topmost(&c.win, false);
                    win::save_position(&c.win);
                }
                // A clean finish needs no attention: close shortly after.
                if state_now == JobState::Completed && s.files_failed == 0 {
                    let w = c.win.clone();
                    glib::timeout_add_local_once(Duration::from_secs(3), move || w.close());
                } else {
                    c.win.present();
                }
            }
        }
        let _ = (&row.widget, &row.title);
    }
    let text = if active == 0 {
        "No active jobs".to_string()
    } else {
        format!(
            "{active} active · {} transferred · {}/s",
            fmt_bytes(bytes),
            fmt_bytes(speed_total as u64)
        )
    };
    summary.set_text(&text);

    let tray = if active == 0 {
        "Tufan".to_string()
    } else if active_known > 0 {
        format!(
            "Tufan — {active} active · {:.0}% · {}/s",
            active_done as f64 * 100.0 / active_known as f64,
            fmt_bytes(speed_total as u64)
        )
    } else {
        format!("Tufan — {active} active")
    };
    if tray != st.tray_status {
        #[cfg(windows)]
        win::set_status(&tray);
        st.tray_status = tray;
    }
}

fn label_for(s: JobState, scanned: bool) -> &'static str {
    match s {
        JobState::Running if !scanned => "Scanning and copying",
        JobState::Running => "Copying",
        JobState::Paused => "Paused",
        JobState::Completed => "Completed",
        JobState::PartiallyCompleted => "Partially completed",
        JobState::Cancelled => "Cancelled",
        JobState::Failed => "Failed",
        JobState::Queued => "Queued",
    }
}

fn fmt_bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

fn fmt_dur(secs: f64) -> String {
    let s = secs.round() as u64;
    if s >= 3600 {
        format!("{}h {:02}m", s / 3600, s / 60 % 60)
    } else if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}
