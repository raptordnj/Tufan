//! Tufan desktop UI (GTK 4). All filesystem work runs in the engine's
//! threads; the UI polls aggregated progress snapshots at 4 Hz.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use tufan_core::journal::{self, Journal};
use tufan_core::{ConflictPolicy, IntegrityMode, JobHandle, JobKind, JobSpec, JobState, Options};

const APP_ID: &str = "org.tufan.Tufan";

const CSS: &str = "
window { background: @window_bg_color; }
.brand { font-size: 20px; font-weight: 600; letter-spacing: 0.5px; }
.muted { opacity: 0.65; }
.job { padding: 10px 12px; border-bottom: 1px solid alpha(currentColor, 0.08); }
.job-title { font-weight: 600; }
.summary { padding: 6px 12px; }
progressbar trough { min-height: 6px; border-radius: 3px; }
progressbar progress { min-height: 6px; border-radius: 3px; }
";

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
    last: (Instant, u64),
    speed: f64,
}

struct App {
    journal: Arc<Journal>,
    sources: Vec<PathBuf>,
    dest: Option<PathBuf>,
    jobs: Vec<JobRow>,
}

type Shared = Rc<RefCell<App>>;

fn main() -> glib::ExitCode {
    let app = gtk4::Application::builder().application_id(APP_ID).build();
    app.connect_activate(activate);
    app.run()
}

fn activate(app: &gtk4::Application) {
    // Single instance: a second launch re-presents the existing window.
    if let Some(w) = app.active_window() {
        w.present();
        return;
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
            s.set_gtk_application_prefer_dark_theme(b.is_active());
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
    paste.set_tooltip_text(Some("Import files copied in the file manager"));
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
                        let mut st = state.borrow_mut();
                        for f in list.files() {
                            if let Some(p) = f.path() {
                                if !st.sources.contains(&p) {
                                    st.sources.push(p);
                                }
                            }
                        }
                    }
                }
                refresh();
            });
        });
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
        let (state, jobs_box, integrity, conflict, win, refresh) = (
            state.clone(),
            jobs_box.clone(),
            integrity.clone(),
            conflict.clone(),
            window.clone(),
            refresh_src.clone(),
        );
        btn.connect_clicked(move |_| {
            let (sources, dest, journal) = {
                let st = state.borrow();
                (st.sources.clone(), st.dest.clone(), st.journal.clone())
            };
            let Some(dest) = dest else {
                alert(&win, "Choose a destination folder first.");
                return;
            };
            if sources.is_empty() {
                alert(&win, "Add at least one file or folder to transfer.");
                return;
            }
            let options = Options {
                integrity: [
                    IntegrityMode::Verified,
                    IntegrityMode::DurableVerified,
                    IntegrityMode::Fast,
                ][integrity.selected() as usize % 3],
                conflict: [
                    ConflictPolicy::Fail,
                    ConflictPolicy::Skip,
                    ConflictPolicy::Rename,
                    ConflictPolicy::Overwrite,
                ][conflict.selected() as usize % 4],
                ..Options::default()
            };
            match tufan_core::start(
                journal,
                JobSpec {
                    kind,
                    sources,
                    dest_dir: dest,
                    options,
                },
            ) {
                Ok(h) => {
                    add_job_row(&state, &jobs_box, Some(Arc::new(h)), None);
                    state.borrow_mut().sources.clear();
                    refresh();
                }
                Err(e) => alert(&win, &e.to_string()),
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
        add_job_row(&state, &jobs_box, None, Some(j));
    }

    {
        let (state, summary) = (state.clone(), summary.clone());
        glib::timeout_add_local(Duration::from_millis(250), move || {
            tick(&state, &summary);
            glib::ControlFlow::Continue
        });
    }

    // Keep running while jobs are active even if the window is closed.
    {
        let state = state.clone();
        let app = app.clone();
        window.connect_close_request(move |w| {
            let active = state
                .borrow()
                .jobs
                .iter()
                .any(|r| r.handle.as_ref().is_some_and(|h| !h.is_finished()));
            if active {
                w.set_visible(false);
                let guard = app.hold();
                let state = state.clone();
                glib::timeout_add_local(Duration::from_millis(500), move || {
                    let busy = state
                        .borrow()
                        .jobs
                        .iter()
                        .any(|r| r.handle.as_ref().is_some_and(|h| !h.is_finished()));
                    if busy {
                        let _ = &guard;
                        glib::ControlFlow::Continue
                    } else {
                        glib::ControlFlow::Break
                    }
                });
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }

    window.present();
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

fn alert(win: &gtk4::ApplicationWindow, msg: &str) {
    let d = gtk4::AlertDialog::builder()
        .message(msg)
        .modal(true)
        .build();
    d.show(Some(win));
}

fn add_job_row(
    state: &Shared,
    jobs_box: &gtk4::Box,
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
        pause.upcast_ref(),
        cancel.upcast_ref(),
        retry.upcast_ref(),
    ] {
        controls.append(w);
    }
    widget.append(&title);
    widget.append(&bar);
    widget.append(&controls);
    jobs_box.append(&widget);

    let idx = state.borrow().jobs.len();
    {
        let state = state.clone();
        pause.connect_clicked(move |b| {
            let st = state.borrow();
            if let Some(h) = &st.jobs[idx].handle {
                if h.state() == JobState::Paused {
                    h.resume();
                    b.set_label("Pause");
                } else {
                    h.pause();
                    b.set_label("Resume");
                }
            }
        });
    }
    {
        let state = state.clone();
        cancel.connect_clicked(move |_| {
            if let Some(h) = &state.borrow().jobs[idx].handle {
                h.cancel();
            }
        });
    }
    {
        let state = state.clone();
        retry.connect_clicked(move |_| {
            let mut st = state.borrow_mut();
            let journal = st.journal.clone();
            let row = &mut st.jobs[idx];
            match tufan_core::resume(journal, row.job_id) {
                Ok(h) => {
                    row.handle = Some(Arc::new(h));
                    row.retry.set_visible(false);
                    row.pause.set_visible(true);
                    row.pause.set_label("Pause");
                    row.cancel.set_visible(true);
                }
                Err(e) => row.status.set_text(&format!("Cannot resume: {e}")),
            }
        });
    }

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
        last: (Instant::now(), 0),
        speed: 0.0,
    });
}

fn tick(state: &Shared, summary: &gtk4::Label) {
    let mut st = state.borrow_mut();
    let (mut active, mut bytes, mut speed_total) = (0, 0u64, 0f64);
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
        if s.scan_complete && s.bytes_known > 0 {
            row.bar
                .set_fraction((s.bytes_done as f64 / s.bytes_known as f64).min(1.0));
        } else if s.scan_complete {
            row.bar.set_fraction(if finished { 1.0 } else { 0.0 });
        } else if !finished {
            row.bar.pulse();
        }
        let total = if s.scan_complete {
            s.files_discovered.to_string()
        } else {
            format!("{}+", s.files_discovered)
        };
        let mut text = format!(
            "{} · {}/{} files · {} of {}",
            label_for(state_now, s.scan_complete),
            s.files_done + s.files_skipped,
            total,
            fmt_bytes(s.bytes_done),
            if s.scan_complete {
                fmt_bytes(s.bytes_known)
            } else {
                "…".into()
            }
        );
        if !finished && state_now == JobState::Running {
            text += &format!(" · {}/s", fmt_bytes(row.speed as u64));
            if s.scan_complete && row.speed > 1024.0 && s.bytes_known > s.bytes_done {
                let secs = (s.bytes_known - s.bytes_done) as f64 / row.speed;
                if secs > 3.0 {
                    text += &format!(" · about {} left", fmt_dur(secs));
                }
            }
            active += 1;
            speed_total += row.speed;
        }
        if s.files_failed > 0 {
            text += &format!(" · {} failed", s.files_failed);
            if let Some(e) = h.report().errors.first() {
                row.status.set_tooltip_text(Some(e));
            }
        }
        row.status.set_text(&text);
        bytes += s.bytes_done;
        if finished {
            row.pause.set_visible(false);
            row.cancel.set_visible(false);
            row.retry.set_label("Retry Failed");
            row.retry.set_visible(
                s.files_failed > 0 || matches!(state_now, JobState::Cancelled | JobState::Failed),
            );
        }
        let _ = (&row.widget, &row.title);
    }
    summary.set_text(&if active == 0 {
        "No active jobs".to_string()
    } else {
        format!(
            "{active} active · {} transferred · {}/s",
            fmt_bytes(bytes),
            fmt_bytes(speed_total as u64)
        )
    });
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
