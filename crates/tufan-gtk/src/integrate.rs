//! File manager actions on Linux and macOS: "Tufan Copy", "Tufan Cut" and
//! "Paste with Tufan". Another app's Ctrl+V cannot be intercepted there
//! (Wayland forbids it), so these actions are the integration point.
//!
//! Everything is per user, written on each start of the app (so the paths
//! follow the installed binary) and only for file managers that are
//! present. `tufan-gtk --remove-integration` deletes it again.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Marker in every file we own; `remove` only deletes files carrying it.
const MARK: &str = "Installed by Tufan";

struct File {
    path: PathBuf,
    body: String,
    exec: bool,
}

pub fn install() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    for f in files(&exe) {
        if fs::read_to_string(&f.path).ok().as_deref() == Some(&f.body) {
            continue;
        }
        if let Some(dir) = f.path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&f.path, &f.body)?;
        if f.exec {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&f.path, fs::Permissions::from_mode(0o755))?;
        }
    }
    #[cfg(target_os = "linux")]
    add_nautilus_shortcuts()?;
    #[cfg(target_os = "macos")]
    {
        // Make Finder pick up new or changed Quick Actions.
        let _ = std::process::Command::new("/System/Library/CoreServices/pbs")
            .arg("-update")
            .status();
    }
    Ok(())
}

pub fn remove() {
    let exe = std::env::current_exe().unwrap_or_default();
    for f in files(&exe) {
        if fs::read_to_string(&f.path).is_ok_and(|s| s.contains(MARK)) {
            let _ = fs::remove_file(&f.path);
        }
    }
    #[cfg(target_os = "macos")]
    for name in SERVICES.map(|s| s.0) {
        let _ = fs::remove_dir_all(home().join(format!("Library/Services/{name}.workflow")));
    }
    #[cfg(target_os = "linux")]
    {
        let accels = config_home().join("nautilus/scripts-accels");
        if let Ok(s) = fs::read_to_string(&accels) {
            let kept: String = s
                .lines()
                .filter(|l| !SCRIPTS.iter().any(|(name, ..)| l.ends_with(name)))
                .map(|l| format!("{l}\n"))
                .collect();
            let _ = fs::write(accels, kept);
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
}

#[cfg(target_os = "linux")]
fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"))
}

#[cfg(target_os = "linux")]
fn on_path(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
}

/// Double-quoted for sh.
fn sh_quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    let mut out = String::from("\"");
    for c in s.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// (script name, tufan-gtk arguments, Nautilus shortcut)
#[cfg(target_os = "linux")]
const SCRIPTS: [(&str, &str, &str); 3] = [
    ("Tufan Copy", "--copy", "<Control><Shift>c"),
    ("Tufan Cut", "--cut", "<Control><Shift>x"),
    ("Paste with Tufan", "--paste", "<Control><Shift>v"),
];

/// Nautilus and Caja run scripts in the current folder with the selection
/// as arguments; paste goes into a single selected folder, else here.
#[cfg(target_os = "linux")]
fn script(exe: &Path, arg: &str, uri_var: &str) -> String {
    let exe = sh_quote(exe);
    let body = if arg == "--paste" {
        format!(
            "if [ $# -eq 1 ] && [ -d \"$1\" ]; then exec {exe} --paste \"$1\"; fi\n\
             exec {exe} --paste \"${{{uri_var}:-$PWD}}\"\n"
        )
    } else {
        format!("exec {exe} {arg} \"$@\"\n")
    };
    format!("#!/bin/sh\n# {MARK}; `tufan-gtk --remove-integration` removes it.\n{body}")
}

#[cfg(target_os = "linux")]
fn files(exe: &Path) -> Vec<File> {
    let mut out = Vec::new();
    let data = data_home();
    if on_path("nautilus") {
        for (name, arg, _) in SCRIPTS {
            out.push(File {
                path: data.join("nautilus/scripts").join(name),
                body: script(exe, arg, "NAUTILUS_SCRIPT_CURRENT_URI"),
                exec: true,
            });
        }
    }
    if on_path("caja") {
        for (name, arg, _) in SCRIPTS {
            out.push(File {
                path: config_home().join("caja/scripts").join(name),
                body: script(exe, arg, "CAJA_SCRIPT_CURRENT_URI"),
                exec: true,
            });
        }
    }
    // Desktop-entry Exec quoting.
    let exec = sh_quote(exe);
    if on_path("dolphin") {
        // KDE only runs executable service menus from the user's folder.
        out.push(File {
            path: data.join("kio/servicemenus/tufan.desktop"),
            body: format!(
                "# {MARK}\n[Desktop Entry]\nType=Service\n\
                 X-KDE-ServiceTypes=KonqPopupMenu/Plugin\nMimeType=all/all;\n\
                 Actions=tufanCopy;tufanCut;\n\n\
                 [Desktop Action tufanCopy]\nName=Tufan Copy\nIcon=edit-copy\n\
                 Exec={exec} --copy %F\n\n\
                 [Desktop Action tufanCut]\nName=Tufan Cut\nIcon=edit-cut\n\
                 Exec={exec} --cut %F\n"
            ),
            exec: true,
        });
        out.push(File {
            path: data.join("kio/servicemenus/tufan-paste.desktop"),
            body: format!(
                "# {MARK}\n[Desktop Entry]\nType=Service\n\
                 X-KDE-ServiceTypes=KonqPopupMenu/Plugin\nMimeType=inode/directory;\n\
                 Actions=tufanPaste;\n\n\
                 [Desktop Action tufanPaste]\nName=Paste with Tufan\nIcon=edit-paste\n\
                 Exec={exec} --paste %f\n"
            ),
            exec: true,
        });
    }
    if on_path("nemo") {
        let action =
            |file: &str, name: &str, icon: &str, args: &str, selection: &str, ext: &str| File {
                path: data.join("nemo/actions").join(file),
                body: format!(
                    "# {MARK}\n[Nemo Action]\nName={name}\nIcon-Name={icon}\n\
                     Exec={exec} {args}\nSelection={selection}\nExtensions={ext};\n"
                ),
                exec: false,
            };
        out.push(action(
            "tufan-copy.nemo_action",
            "Tufan Copy",
            "edit-copy",
            "--copy %F",
            "notnone",
            "any",
        ));
        out.push(action(
            "tufan-cut.nemo_action",
            "Tufan Cut",
            "edit-cut",
            "--cut %F",
            "notnone",
            "any",
        ));
        out.push(action(
            "tufan-paste.nemo_action",
            "Paste with Tufan",
            "edit-paste",
            "--paste %P",
            "none",
            "any",
        ));
        out.push(action(
            "tufan-paste-into.nemo_action",
            "Paste with Tufan",
            "edit-paste",
            "--paste %F",
            "s",
            "dir",
        ));
    }
    out
}

/// Adds Ctrl+Shift+C/X/V for the Nautilus scripts unless the user already
/// bound those scripts or those keys.
#[cfg(target_os = "linux")]
fn add_nautilus_shortcuts() -> io::Result<()> {
    if !on_path("nautilus") {
        return Ok(());
    }
    let path = config_home().join("nautilus/scripts-accels");
    let current = fs::read_to_string(&path).unwrap_or_default();
    let mut add = String::new();
    for (name, _, accel) in SCRIPTS {
        let taken = current.lines().any(|l| {
            let l = l.trim();
            l.ends_with(name) || l.split_whitespace().next() == Some(accel)
        });
        if !taken {
            add += &format!("{accel} {name}\n");
        }
    }
    if add.is_empty() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let sep = if current.is_empty() || current.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    fs::write(path, format!("{current}{sep}{add}"))
}

/// (Quick Action name, tufan-gtk arguments)
#[cfg(target_os = "macos")]
const SERVICES: [(&str, &str); 3] = [
    ("Tufan Copy", "--copy \"$@\""),
    ("Tufan Cut", "--cut \"$@\""),
    // A selected file pastes into its folder.
    ("Paste with Tufan", "--paste \"$1\""),
];

/// Finder Quick Actions: Automator "Run Shell Script" workflows that get
/// the selected items as arguments.
#[cfg(target_os = "macos")]
fn files(exe: &Path) -> Vec<File> {
    let services = home().join("Library/Services");
    let mut out = Vec::new();
    for (name, args) in SERVICES {
        let command = xml_escape(&format!("exec {} {args}", sh_quote(exe)));
        let dir = services.join(format!("{name}.workflow/Contents"));
        out.push(File {
            path: dir.join("Info.plist"),
            body: format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- {MARK} -->
<plist version="1.0">
<dict>
	<key>NSServices</key>
	<array>
		<dict>
			<key>NSMenuItem</key>
			<dict><key>default</key><string>{name}</string></dict>
			<key>NSMessage</key>
			<string>runWorkflowAsService</string>
			<key>NSRequiredContext</key>
			<dict><key>NSApplicationIdentifier</key><string>com.apple.finder</string></dict>
			<key>NSSendFileTypes</key>
			<array><string>public.item</string></array>
		</dict>
	</array>
</dict>
</plist>
"#
            ),
            exec: false,
        });
        out.push(File {
            path: dir.join("document.wflow"),
            body: format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- {MARK} -->
<plist version="1.0">
<dict>
	<key>AMApplicationBuild</key><string>523</string>
	<key>AMApplicationVersion</key><string>2.10</string>
	<key>AMDocumentVersion</key><string>2</string>
	<key>actions</key>
	<array>
		<dict>
			<key>action</key>
			<dict>
				<key>AMAccepts</key>
				<dict>
					<key>Container</key><string>List</string>
					<key>Optional</key><true/>
					<key>Types</key><array><string>com.apple.cocoa.string</string></array>
				</dict>
				<key>AMActionVersion</key><string>2.0.3</string>
				<key>AMApplication</key><array><string>Automator</string></array>
				<key>AMParameterProperties</key>
				<dict>
					<key>COMMAND_STRING</key><dict/>
					<key>CheckedForUserDefaultShell</key><dict/>
					<key>inputMethod</key><dict/>
					<key>shell</key><dict/>
					<key>source</key><dict/>
				</dict>
				<key>AMProvides</key>
				<dict>
					<key>Container</key><string>List</string>
					<key>Types</key><array><string>com.apple.cocoa.string</string></array>
				</dict>
				<key>ActionBundlePath</key><string>/System/Library/Automator/Run Shell Script.action</string>
				<key>ActionName</key><string>Run Shell Script</string>
				<key>ActionParameters</key>
				<dict>
					<key>COMMAND_STRING</key><string>{command}</string>
					<key>CheckedForUserDefaultShell</key><true/>
					<key>inputMethod</key><integer>1</integer>
					<key>shell</key><string>/bin/sh</string>
					<key>source</key><string></string>
				</dict>
				<key>BundleIdentifier</key><string>com.apple.RunShellScript</string>
				<key>CFBundleVersion</key><string>2.0.3</string>
				<key>CanShowSelectedItemsWhenRun</key><false/>
				<key>CanShowWhenRun</key><true/>
				<key>Category</key><array><string>AMCategoryUtilities</string></array>
				<key>Class Name</key><string>RunShellScriptAction</string>
				<key>InputUUID</key><string>5B6A3B0E-6F0C-4C6E-9D53-0A1F2B3C4D51</string>
				<key>Keywords</key><array><string>Shell</string><string>Script</string></array>
				<key>OutputUUID</key><string>5B6A3B0E-6F0C-4C6E-9D53-0A1F2B3C4D52</string>
				<key>UUID</key><string>5B6A3B0E-6F0C-4C6E-9D53-0A1F2B3C4D53</string>
				<key>UnlocalizedApplications</key><array><string>Automator</string></array>
				<key>isViewVisible</key><integer>1</integer>
			</dict>
			<key>isViewVisible</key><integer>1</integer>
		</dict>
	</array>
	<key>connectors</key><dict/>
	<key>workflowMetaData</key>
	<dict>
		<key>applicationBundleIDsByPath</key><dict/>
		<key>applicationPaths</key><array/>
		<key>inputTypeIdentifier</key><string>com.apple.Automator.fileSystemObject</string>
		<key>outputTypeIdentifier</key><string>com.apple.Automator.nothing</string>
		<key>presentationMode</key><integer>15</integer>
		<key>processesInput</key><integer>0</integer>
		<key>serviceInputTypeIdentifier</key><string>com.apple.Automator.fileSystemObject</string>
		<key>serviceOutputTypeIdentifier</key><string>com.apple.Automator.nothing</string>
		<key>serviceProcessesInput</key><integer>0</integer>
		<key>systemImageName</key><string>NSActionTemplate</string>
		<key>useAutomaticInputType</key><integer>0</integer>
		<key>workflowTypeIdentifier</key><string>com.apple.Automator.servicesMenu</string>
	</dict>
</dict>
</plist>
"#
            ),
            exec: false,
        });
    }
    out
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn files(_exe: &Path) -> Vec<File> {
    Vec::new()
}
