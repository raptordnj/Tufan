//! Windows integration: tray icon, single instance, settings in
//! HKCU\Software\Tufan, and the opt-in Explorer paste takeover.
//!
//! Explorer paste works without loading anything into Explorer: a
//! low-level keyboard hook claims Ctrl+V only while the file view of an
//! Explorer window has focus and the clipboard holds files. A worker then
//! resolves that tab's folder through the shell automation interfaces. If
//! anything cannot be resolved (virtual folders, libraries, a locked
//! clipboard) the keystroke is replayed so Explorer pastes as usual.

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::Duration;

use gtk4::glib;
use tufan_core::JobKind;
use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, HBRUSH, MONITOR_DEFAULTTONULL};
use windows::Win32::System::Com::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{CF_HDROP, DROPEFFECT_COPY, DROPEFFECT_MOVE};
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::*;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub enum Event {
    Show,
    PauseAll,
    ResumeAll,
    Exit,
    Paste {
        kind: JobKind,
        sources: Vec<PathBuf>,
        dest: PathBuf,
    },
}

const KEY: PCWSTR = w!("Software\\Tufan");
const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const SHOW_EVENT: PCWSTR = w!("Local\\org.tufan.Tufan.Show");
const WM_TRAY: u32 = WM_APP + 1;
/// Marks keystrokes we replay so the hook lets them through.
const REPLAY_MARK: usize = 0x5475_6661;

static EXPLORER_PASTE: AtomicBool = AtomicBool::new(false);
static V_SWALLOWED: AtomicBool = AtomicBool::new(false);
static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);
/// (top-level window, file view) as raw handles: HWND is not `Send`.
static PASTE_TX: OnceLock<mpsc::SyncSender<(isize, isize)>> = OnceLock::new();

type Handler = Box<dyn Fn(Event)>;

thread_local! {
    static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
}

/// Runs `ev` on the GTK main thread.
fn dispatch(ev: Event) {
    let ev = std::sync::Mutex::new(Some(ev));
    glib::MainContext::default().invoke(move || {
        if let Some(ev) = ev.lock().unwrap().take() {
            HANDLER.with(|h| {
                if let Some(h) = h.borrow().as_ref() {
                    h(ev)
                }
            });
        }
    });
}

// --- Single instance ------------------------------------------------------

/// Returns false when another instance is running; it has been asked to
/// show its window and this process should exit.
pub fn claim_single_instance() -> bool {
    unsafe {
        let Ok(ev) = CreateEventW(None, false, false, SHOW_EVENT) else {
            return true;
        };
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = SetEvent(ev);
            let _ = CloseHandle(ev);
            return false;
        }
        let ev = ev.0 as isize;
        std::thread::spawn(move || loop {
            if WaitForSingleObject(HANDLE(ev as *mut c_void), INFINITE) == WAIT_OBJECT_0 {
                dispatch(Event::Show);
            } else {
                return;
            }
        });
        true
    }
}

// --- Settings --------------------------------------------------------------

fn get_dword(name: PCWSTR) -> Option<u32> {
    let mut v = 0u32;
    let mut len = 4u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            KEY,
            name,
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut u32 as *mut c_void),
            Some(&mut len),
        )
        .ok()
        .ok()
        .map(|_| v)
    }
}

fn set_dword(name: PCWSTR, v: u32) {
    unsafe {
        let _ = RegSetKeyValueW(
            HKEY_CURRENT_USER,
            KEY,
            name,
            REG_DWORD.0,
            Some(&v as *const u32 as *const c_void),
            4,
        );
    }
}

pub fn close_to_tray() -> bool {
    get_dword(w!("CloseToTray")).unwrap_or(1) != 0
}

fn starts_at_login() -> bool {
    let mut len = 0u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            w!("Tufan"),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut len),
        )
        .is_ok()
    }
}

fn set_starts_at_login(on: bool) {
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else {
                return;
            };
            let cmd = wide(&format!("\"{}\" --background", exe.display()));
            let _ = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                w!("Tufan"),
                REG_SZ.0,
                Some(cmd.as_ptr() as *const c_void),
                (cmd.len() * 2) as u32,
            );
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, w!("Tufan"));
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// --- Tray and hook thread ---------------------------------------------------

/// Starts the tray icon and, when enabled, the Explorer paste hook.
/// `handler` receives every event on the GTK main thread.
pub fn init(handler: Handler) {
    HANDLER.with(|h| *h.borrow_mut() = Some(handler));
    EXPLORER_PASTE.store(
        get_dword(w!("ExplorerPaste")).unwrap_or(0) != 0,
        Ordering::Relaxed,
    );
    let (tx, rx) = mpsc::sync_channel(4);
    let _ = PASTE_TX.set(tx);
    std::thread::spawn(move || paste_worker(rx));
    // The hook and the tray window share one thread with its own message
    // loop, so neither depends on GTK's main loop staying responsive.
    std::thread::spawn(|| unsafe { shell_thread() });
}

/// Updates the tray tooltip (at most 127 characters are shown).
pub fn set_status(text: &str) {
    let hwnd = HWND(TRAY_HWND.load(Ordering::Relaxed) as *mut c_void);
    if hwnd.is_invalid() {
        return;
    }
    let mut nid = notify_data(hwnd);
    nid.uFlags = NIF_TIP;
    fill_tip(&mut nid, text);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// Removes the tray icon; Windows leaves a stale one behind otherwise.
pub fn remove_tray() {
    let hwnd = HWND(TRAY_HWND.swap(0, Ordering::Relaxed) as *mut c_void);
    if !hwnd.is_invalid() {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &notify_data(hwnd));
        }
    }
}

fn notify_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    }
}

fn fill_tip(nid: &mut NOTIFYICONDATAW, text: &str) {
    let w: Vec<u16> = text.encode_utf16().take(nid.szTip.len() - 1).collect();
    nid.szTip[..w.len()].copy_from_slice(&w);
    nid.szTip[w.len()] = 0;
}

unsafe fn add_icon(hwnd: HWND) {
    let hinst = GetModuleHandleW(None).unwrap_or_default();
    let icon = LoadImageW(
        Some(hinst.into()),
        // MAKEINTRESOURCE(1): the icon in tufan.rc.
        PCWSTR(std::ptr::without_provenance(1)),
        IMAGE_ICON,
        GetSystemMetrics(SM_CXSMICON),
        GetSystemMetrics(SM_CYSMICON),
        LR_DEFAULTCOLOR,
    )
    .map(|h| HICON(h.0))
    .or_else(|_| LoadIconW(None, IDI_APPLICATION))
    .unwrap_or_default();
    let mut nid = notify_data(hwnd);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = icon;
    fill_tip(&mut nid, "Tufan");
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
}

unsafe fn shell_thread() {
    let hinst = GetModuleHandleW(None).unwrap_or_default();
    let class = w!("TufanTray");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(tray_proc),
        hInstance: hinst.into(),
        lpszClassName: class,
        hbrBackground: HBRUSH::default(),
        ..Default::default()
    };
    RegisterClassW(&wc);
    // A hidden top-level window (not message-only) so it receives the
    // TaskbarCreated broadcast after Explorer restarts.
    let Ok(hwnd) = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class,
        w!("Tufan tray"),
        WINDOW_STYLE::default(),
        0,
        0,
        0,
        0,
        None,
        None,
        Some(hinst.into()),
        None,
    ) else {
        return;
    };
    TRAY_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
    add_icon(hwnd);
    let _hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), Some(hinst.into()), 0);
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

fn taskbar_created() -> u32 {
    static MSG_ID: OnceLock<u32> = OnceLock::new();
    *MSG_ID.get_or_init(|| unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) })
}

unsafe extern "system" fn tray_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_TRAY {
        match lp.0 as u32 {
            WM_LBUTTONUP | WM_LBUTTONDBLCLK => dispatch(Event::Show),
            WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
            _ => {}
        }
        return LRESULT(0);
    }
    if msg == taskbar_created() {
        add_icon(hwnd);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

unsafe fn show_menu(hwnd: HWND) {
    const OPEN: usize = 1;
    const PAUSE: usize = 2;
    const RESUME: usize = 3;
    const PASTE: usize = 4;
    const LOGIN: usize = 5;
    const TRAY: usize = 6;
    const EXIT: usize = 7;
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
    let _ = AppendMenuW(menu, MF_STRING, OPEN, w!("Open Tufan"));
    let _ = SetMenuDefaultItem(menu, OPEN as u32, 0);
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(menu, MF_STRING, PAUSE, w!("Pause all"));
    let _ = AppendMenuW(menu, MF_STRING, RESUME, w!("Resume all"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(
        menu,
        MF_STRING | check(EXPLORER_PASTE.load(Ordering::Relaxed)),
        PASTE,
        w!("Use Tufan for Explorer paste (Ctrl+V)"),
    );
    let _ = AppendMenuW(
        menu,
        MF_STRING | check(starts_at_login()),
        LOGIN,
        w!("Start with Windows"),
    );
    let _ = AppendMenuW(
        menu,
        MF_STRING | check(close_to_tray()),
        TRAY,
        w!("Close to tray"),
    );
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(menu, MF_STRING, EXIT, w!("Exit"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // Required so the menu closes when the user clicks elsewhere.
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
        pt.x,
        pt.y,
        None,
        hwnd,
        None,
    );
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
    match cmd.0 as usize {
        OPEN => dispatch(Event::Show),
        PAUSE => dispatch(Event::PauseAll),
        RESUME => dispatch(Event::ResumeAll),
        PASTE => {
            let on = !EXPLORER_PASTE.load(Ordering::Relaxed);
            EXPLORER_PASTE.store(on, Ordering::Relaxed);
            set_dword(w!("ExplorerPaste"), on as u32);
            // Takeover only works while Tufan runs.
            if on && !starts_at_login() {
                set_starts_at_login(true);
            }
        }
        LOGIN => set_starts_at_login(!starts_at_login()),
        TRAY => set_dword(w!("CloseToTray"), !close_to_tray() as u32),
        EXIT => dispatch(Event::Exit),
        _ => {}
    }
}

// --- Explorer paste -----------------------------------------------------------

fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn down(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000 != 0 }
}

/// The Explorer file view that has keyboard focus, with its top-level
/// window. Rename boxes, the address bar and the search box are excluded.
fn focused_explorer_view() -> Option<(HWND, HWND)> {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() || class_of(fg) != "CabinetWClass" {
            return None;
        }
        let tid = GetWindowThreadProcessId(fg, None);
        let mut gti = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        GetGUIThreadInfo(tid, &mut gti).ok()?;
        let mut h = gti.hwndFocus;
        if h.is_invalid() || class_of(h) == "Edit" {
            return None;
        }
        while !h.is_invalid() && h != fg {
            if class_of(h) == "SHELLDLL_DefView" {
                return Some((fg, h));
            }
            h = GetParent(h).unwrap_or_default();
        }
        None
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        if k.vkCode == u32::from(b'V') && k.dwExtraInfo != REPLAY_MARK {
            let swallow = match wp.0 as u32 {
                // Auto-repeat of a claimed keystroke is swallowed too.
                WM_KEYDOWN => {
                    V_SWALLOWED.load(Ordering::Relaxed)
                        || (claim_paste() && !V_SWALLOWED.swap(true, Ordering::Relaxed))
                }
                WM_KEYUP => V_SWALLOWED.swap(false, Ordering::Relaxed),
                _ => false,
            };
            if swallow {
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

/// Cheap checks only: this runs inside the keyboard hook.
fn claim_paste() -> bool {
    if !EXPLORER_PASTE.load(Ordering::Relaxed)
        || !down(VK_CONTROL)
        || down(VK_SHIFT)
        || down(VK_MENU)
        || down(VK_LWIN)
        || down(VK_RWIN)
    {
        return false;
    }
    let has_files = unsafe { IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok() };
    if !has_files {
        return false;
    }
    let Some(view) = focused_explorer_view() else {
        return false;
    };
    PASTE_TX.get().is_some_and(|tx| {
        tx.try_send((view.0 .0 as isize, view.1 .0 as isize))
            .is_ok()
    })
}

fn paste_worker(rx: mpsc::Receiver<(isize, isize)>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    for (top, view) in rx {
        let (top, view) = (HWND(top as *mut c_void), HWND(view as *mut c_void));
        let request = read_clipboard().and_then(|(kind, sources, seq)| {
            let dest = unsafe { view_folder(top, view) }?;
            Some((kind, sources, seq, dest))
        });
        match request {
            Some((kind, sources, seq, dest)) => {
                // Explorer empties the clipboard after a cut is pasted; do
                // the same, but only if nothing else was copied meanwhile.
                if kind == JobKind::Move {
                    unsafe {
                        if GetClipboardSequenceNumber() == seq && open_clipboard() {
                            let _ = EmptyClipboard();
                            let _ = CloseClipboard();
                        }
                    }
                }
                dispatch(Event::Paste {
                    kind,
                    sources,
                    dest,
                });
            }
            None => replay_ctrl_v(),
        }
    }
}

fn open_clipboard() -> bool {
    for _ in 0..10 {
        if unsafe { OpenClipboard(None) }.is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Files on the clipboard, whether they were cut, and the clipboard
/// sequence number they were read at. The clipboard is not modified.
fn read_clipboard() -> Option<(JobKind, Vec<PathBuf>, u32)> {
    unsafe {
        if !open_clipboard() {
            return None;
        }
        let seq = GetClipboardSequenceNumber();
        let mut sources = Vec::new();
        if let Ok(h) = GetClipboardData(CF_HDROP.0 as u32) {
            let drop = HDROP(h.0);
            let n = DragQueryFileW(drop, u32::MAX, None);
            for i in 0..n {
                let len = DragQueryFileW(drop, i, None) as usize;
                let mut buf = vec![0u16; len + 1];
                let got = DragQueryFileW(drop, i, Some(&mut buf)) as usize;
                sources.push(PathBuf::from(String::from_utf16_lossy(&buf[..got])));
            }
        }
        let mut effect = DROPEFFECT_COPY.0;
        let fmt = RegisterClipboardFormatW(w!("Preferred DropEffect"));
        if let Ok(h) = GetClipboardData(fmt) {
            let hg = windows::Win32::Foundation::HGLOBAL(h.0);
            let p = GlobalLock(hg) as *const u32;
            if !p.is_null() {
                effect = *p;
                let _ = GlobalUnlock(hg);
            }
        }
        let _ = CloseClipboard();
        if sources.is_empty() {
            return None;
        }
        let kind = if effect & DROPEFFECT_MOVE.0 != 0 && effect & DROPEFFECT_COPY.0 == 0 {
            JobKind::Move
        } else {
            JobKind::Copy
        };
        Some((kind, sources, seq))
    }
}

/// File-system folder shown by the Explorer tab whose view is `view`.
unsafe fn view_folder(top: HWND, view: HWND) -> Option<PathBuf> {
    let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).ok()?;
    let count = windows.Count().ok()?;
    for i in 0..count {
        let Ok(disp) = windows.Item(&VARIANT::from(i)) else {
            continue;
        };
        let Ok(browser) = disp.cast::<IWebBrowserApp>() else {
            continue;
        };
        // Tabs share one top-level window; compare the active view below.
        if browser.HWND().ok().map(|h| h.0) != Some(top.0 as isize) {
            continue;
        }
        let Ok(sp) = disp.cast::<IServiceProvider>() else {
            continue;
        };
        let Ok(sb) = sp.QueryService::<IShellBrowser>(&SID_STopLevelBrowser) else {
            continue;
        };
        let Ok(sv) = sb.QueryActiveShellView() else {
            continue;
        };
        if sv.GetWindow().ok() != Some(view) {
            continue;
        }
        let fv: IFolderView = sv.cast().ok()?;
        let pf: IPersistFolder2 = fv.GetFolder().ok()?;
        let pidl: *mut ITEMIDLIST = pf.GetCurFolder().ok()?;
        let mut buf = [0u16; 32768];
        let ok = SHGetPathFromIDListEx(pidl, &mut buf, GPFIDL_DEFAULT).as_bool();
        CoTaskMemFree(Some(pidl as *const c_void));
        if !ok {
            return None;
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let p = PathBuf::from(String::from_utf16_lossy(&buf[..n]));
        return p.is_dir().then_some(p);
    }
    None
}

/// Hands a claimed Ctrl+V back to Explorer.
fn replay_ctrl_v() {
    let key = |vk: VIRTUAL_KEY, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                dwExtraInfo: REPLAY_MARK,
                ..Default::default()
            },
        },
    };
    let ctrl_held = down(VK_CONTROL);
    let v = VIRTUAL_KEY(u16::from(b'V'));
    let mut seq = Vec::new();
    if !ctrl_held {
        seq.push(key(VK_CONTROL, false));
    }
    seq.push(key(v, false));
    seq.push(key(v, true));
    if !ctrl_held {
        seq.push(key(VK_CONTROL, true));
    }
    unsafe {
        SendInput(&seq, std::mem::size_of::<INPUT>() as i32);
    }
}

// --- Compact progress windows ---------------------------------------------------

fn hwnd_of(w: &gtk4::Window) -> Option<HWND> {
    use gtk4::prelude::*;
    let surface = w.surface()?;
    let surface = surface.downcast_ref::<gdk4_win32::Win32Surface>()?;
    let h = HWND(surface.handle().0);
    (!h.is_invalid()).then_some(h)
}

/// Keeps a window above all others (GTK 4 has no API for this).
pub fn set_topmost(w: &gtk4::Window, on: bool) {
    let Some(h) = hwnd_of(w) else { return };
    let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
    unsafe {
        let _ = SetWindowPos(
            h,
            Some(after),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// Remembers where the user left a progress window.
pub fn save_position(w: &gtk4::Window) {
    let Some(h) = hwnd_of(w) else { return };
    let mut r = RECT::default();
    if unsafe { GetWindowRect(h, &mut r) }.is_ok() {
        set_dword(w!("CompactX"), r.left as u32);
        set_dword(w!("CompactY"), r.top as u32);
    }
}

/// Moves a progress window to the remembered position, shifted by
/// `offset` pixels so windows of concurrent jobs do not cover each other.
/// A position that is no longer on any monitor is ignored.
pub fn restore_position(w: &gtk4::Window, offset: i32) {
    let (Some(x), Some(y), Some(h)) = (
        get_dword(w!("CompactX")),
        get_dword(w!("CompactY")),
        hwnd_of(w),
    ) else {
        return;
    };
    let (x, y) = (x as i32 + offset, y as i32 + offset);
    // The title bar must land on a monitor so the window can be dragged.
    let probe = POINT {
        x: x + 40,
        y: y + 16,
    };
    if unsafe { MonitorFromPoint(probe, MONITOR_DEFAULTTONULL) }.is_invalid() {
        return;
    }
    unsafe {
        let _ = SetWindowPos(
            h,
            None,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}
