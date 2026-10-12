//! The "Compress with zipc" right-click menu item: installed per user, for
//! the current OS's file manager, running this app in quick mode.
//!
//! - macOS: a Quick Action in `~/Library/Services` (Finder: right-click >
//!   Quick Actions, and the Services menu).
//! - Windows: a verb for files and folders under `HKEY_CURRENT_USER`
//!   (Windows 11: right-click > Show more options). Explorer starts it once
//!   per selected item; quick mode gathers them into one archive.
//! - Linux: a Nautilus (GNOME Files) and Nemo script, and a Dolphin service
//!   menu.

use std::io;
#[cfg(any(target_os = "macos", test))]
use std::path::Path;
use std::path::PathBuf;

/// The menu item's label.
pub const LABEL: &str = "Compress with zipc";

/// Whether this OS is supported at all.
pub const SUPPORTED: bool = cfg!(any(target_os = "macos", windows, target_os = "linux"));

/// How to start this app in quick mode from a file manager.
fn launcher() -> io::Result<PathBuf> {
    // An AppImage runs from a temporary mount; point at the image itself.
    if let Some(p) = std::env::var_os("APPIMAGE") {
        return Ok(PathBuf::from(p));
    }
    std::env::current_exe()
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn workflow_dir() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME is not set"))?;
    Ok(PathBuf::from(home).join(format!("Library/Services/{LABEL}.workflow")))
}

#[cfg(target_os = "macos")]
pub fn installed() -> bool {
    workflow_dir().is_ok_and(|d| d.join("Contents/document.wflow").is_file())
}

/// The Quick Action's shell command: open the app bundle (a new instance, so
/// it works while the app is already open) in quick mode, or run the binary
/// directly for an unbundled development build.
#[cfg(target_os = "macos")]
fn quick_action_command(exe: &Path) -> String {
    let bundle = exe
        .ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .map(Path::to_path_buf);
    match bundle {
        Some(app) => format!(
            "open -n -a {} --args --quick \"$@\"",
            sh_quote(&app.to_string_lossy())
        ),
        None => format!(
            "{} --quick \"$@\" >/dev/null 2>&1 &",
            sh_quote(&exe.to_string_lossy())
        ),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(any(target_os = "macos", test))]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(any(target_os = "macos", test))]
fn workflow_info_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>NSServices</key>
	<array>
		<dict>
			<key>NSBackgroundColorName</key>
			<string>background</string>
			<key>NSIconName</key>
			<string>NSActionTemplate</string>
			<key>NSMenuItem</key>
			<dict>
				<key>default</key>
				<string>{label}</string>
			</dict>
			<key>NSMessage</key>
			<string>runWorkflowAsService</string>
			<key>NSRequiredContext</key>
			<dict>
				<key>NSApplicationIdentifier</key>
				<string>com.apple.finder</string>
			</dict>
			<key>NSSendFileTypes</key>
			<array>
				<string>public.item</string>
			</array>
		</dict>
	</array>
</dict>
</plist>
"#,
        label = xml_escape(LABEL)
    )
}

/// An Automator "Run Shell Script" Quick Action over files and folders in
/// Finder, passing them as arguments to `command`.
#[cfg(any(target_os = "macos", test))]
fn workflow_document(command: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>AMApplicationBuild</key>
	<string>534</string>
	<key>AMApplicationVersion</key>
	<string>2.10</string>
	<key>AMDocumentVersion</key>
	<string>2</string>
	<key>actions</key>
	<array>
		<dict>
			<key>action</key>
			<dict>
				<key>AMAccepts</key>
				<dict>
					<key>Container</key>
					<string>List</string>
					<key>Optional</key>
					<true/>
					<key>Types</key>
					<array>
						<string>com.apple.cocoa.path</string>
					</array>
				</dict>
				<key>AMActionVersion</key>
				<string>2.0.3</string>
				<key>AMApplication</key>
				<array>
					<string>Automator</string>
				</array>
				<key>AMParameterProperties</key>
				<dict>
					<key>COMMAND_STRING</key>
					<dict/>
					<key>CheckedForUserDefaultShell</key>
					<dict/>
					<key>inputMethod</key>
					<dict/>
					<key>shell</key>
					<dict/>
					<key>source</key>
					<dict/>
				</dict>
				<key>AMProvides</key>
				<dict>
					<key>Container</key>
					<string>List</string>
					<key>Types</key>
					<array>
						<string>com.apple.cocoa.string</string>
					</array>
				</dict>
				<key>ActionBundlePath</key>
				<string>/System/Library/Automator/Run Shell Script.action</string>
				<key>ActionName</key>
				<string>Run Shell Script</string>
				<key>ActionParameters</key>
				<dict>
					<key>COMMAND_STRING</key>
					<string>{command}</string>
					<key>CheckedForUserDefaultShell</key>
					<true/>
					<key>inputMethod</key>
					<integer>1</integer>
					<key>shell</key>
					<string>/bin/sh</string>
					<key>source</key>
					<string></string>
				</dict>
				<key>BundleIdentifier</key>
				<string>com.apple.RunShellScript</string>
				<key>CFBundleVersion</key>
				<string>2.0.3</string>
				<key>CanShowSelectedItemsWhenRun</key>
				<false/>
				<key>CanShowWhenRun</key>
				<true/>
				<key>Category</key>
				<array>
					<string>AMCategoryUtilities</string>
				</array>
				<key>Class Name</key>
				<string>RunShellScriptAction</string>
				<key>InputUUID</key>
				<string>5C1D7A2E-6F0B-4B8E-9C61-2E0F3B7D9A10</string>
				<key>Keywords</key>
				<array>
					<string>Shell</string>
					<string>Script</string>
				</array>
				<key>OutputUUID</key>
				<string>8E4B2F91-3A7C-4D05-B1E8-6C9F0A2D4E31</string>
				<key>UUID</key>
				<string>2B7E9C14-8D3F-4A62-95E0-7F1C3D5A8B42</string>
				<key>UnlocalizedApplications</key>
				<array>
					<string>Automator</string>
				</array>
				<key>arguments</key>
				<dict/>
				<key>isViewVisible</key>
				<integer>1</integer>
				<key>location</key>
				<string>309.000000:305.000000</string>
				<key>nibPath</key>
				<string>/System/Library/Automator/Run Shell Script.action/Contents/Resources/Base.lproj/main.nib</string>
			</dict>
			<key>isViewVisible</key>
			<integer>1</integer>
		</dict>
	</array>
	<key>connectors</key>
	<dict/>
	<key>workflowMetaData</key>
	<dict>
		<key>applicationBundleID</key>
		<string>com.apple.finder</string>
		<key>applicationBundleIDsByPath</key>
		<dict>
			<key>/System/Library/CoreServices/Finder.app</key>
			<string>com.apple.finder</string>
		</dict>
		<key>applicationPath</key>
		<string>/System/Library/CoreServices/Finder.app</string>
		<key>applicationPaths</key>
		<array>
			<string>/System/Library/CoreServices/Finder.app</string>
		</array>
		<key>inputTypeIdentifier</key>
		<string>com.apple.Automator.fileSystemObject</string>
		<key>outputTypeIdentifier</key>
		<string>com.apple.Automator.nothing</string>
		<key>presentationMode</key>
		<integer>15</integer>
		<key>processesInput</key>
		<false/>
		<key>serviceApplicationBundleID</key>
		<string>com.apple.finder</string>
		<key>serviceApplicationPath</key>
		<string>/System/Library/CoreServices/Finder.app</string>
		<key>serviceInputTypeIdentifier</key>
		<string>com.apple.Automator.fileSystemObject</string>
		<key>serviceOutputTypeIdentifier</key>
		<string>com.apple.Automator.nothing</string>
		<key>serviceProcessesInput</key>
		<false/>
		<key>systemImageName</key>
		<string>NSActionTemplate</string>
		<key>useAutomaticInputType</key>
		<false/>
		<key>workflowTypeIdentifier</key>
		<string>com.apple.Automator.servicesMenu</string>
	</dict>
</dict>
</plist>
"#,
        command = xml_escape(command)
    )
}

#[cfg(target_os = "macos")]
pub fn install() -> io::Result<()> {
    let dir = workflow_dir()?;
    let contents = dir.join("Contents");
    std::fs::create_dir_all(&contents)?;
    std::fs::write(contents.join("Info.plist"), workflow_info_plist())?;
    let command = quick_action_command(&launcher()?);
    std::fs::write(contents.join("document.wflow"), workflow_document(&command))?;
    refresh_services();
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn uninstall() -> io::Result<()> {
    let dir = workflow_dir()?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    refresh_services();
    Ok(())
}

/// Make Finder notice the change now rather than at next login.
#[cfg(target_os = "macos")]
fn refresh_services() {
    let _ = std::process::Command::new("/System/Library/CoreServices/pbs")
        .arg("-update")
        .output();
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
const VERB_KEYS: [&str; 2] = [
    r"HKCU\Software\Classes\*\shell\zipc.compress",
    r"HKCU\Software\Classes\Directory\shell\zipc.compress",
];

#[cfg(windows)]
fn reg(args: &[&str]) -> io::Result<bool> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("reg")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;
    Ok(out.status.success())
}

#[cfg(windows)]
pub fn installed() -> bool {
    VERB_KEYS
        .iter()
        .all(|k| reg(&["query", &format!(r"{k}\command")]).unwrap_or(false))
}

#[cfg(windows)]
pub fn install() -> io::Result<()> {
    let exe = launcher()?;
    let exe = exe.to_string_lossy();
    let command = format!("\"{exe}\" --quick \"%1\"");
    let icon = format!("\"{exe}\",0");
    for key in VERB_KEYS {
        let ok = reg(&["add", key, "/ve", "/d", LABEL, "/f"])?
            && reg(&["add", key, "/v", "Icon", "/d", &icon, "/f"])?
            && reg(&[
                "add",
                &format!(r"{key}\command"),
                "/ve",
                "/d",
                &command,
                "/f",
            ])?;
        if !ok {
            return Err(io::Error::other(format!("could not write {key}")));
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn uninstall() -> io::Result<()> {
    for key in VERB_KEYS {
        // Fails (harmlessly) if it isn't there.
        reg(&["delete", key, "/f"])?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn data_home() -> io::Result<PathBuf> {
    if let Some(d) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(d));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".local/share"))
}

#[cfg(target_os = "linux")]
fn linux_files() -> io::Result<[PathBuf; 3]> {
    let d = data_home()?;
    Ok([
        d.join(format!("nautilus/scripts/{LABEL}")),
        d.join(format!("nemo/scripts/{LABEL}")),
        d.join("kio/servicemenus/zipc-compress.desktop"),
    ])
}

#[cfg(target_os = "linux")]
pub fn installed() -> bool {
    linux_files().is_ok_and(|f| f.iter().any(|p| p.is_file()))
}

#[cfg(target_os = "linux")]
pub fn install() -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let exe = launcher()?;
    let exe = exe.to_string_lossy();
    let script = format!("#!/bin/sh\nexec {} --quick \"$@\"\n", sh_quote(&exe));
    // Dolphin's Exec quoting: double quotes, with \ " ` $ escaped.
    let exec_quoted = exe
        .replace('\\', r"\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$");
    let desktop = format!(
        "[Desktop Entry]\nType=Service\nMimeType=application/octet-stream;inode/directory;all/allfiles;\n\
         Actions=compress\nX-KDE-Priority=TopLevel\n\n[Desktop Action compress]\nName={LABEL}\n\
         Icon=package-x-generic\nExec=\"{exec_quoted}\" --quick %F\n"
    );
    let [nautilus, nemo, dolphin] = linux_files()?;
    for (path, body) in [(&nautilus, &script), (&nemo, &script), (&dolphin, &desktop)] {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, body)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn uninstall() -> io::Result<()> {
    for p in linux_files()? {
        match std::fs::remove_file(&p) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Elsewhere
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
pub fn installed() -> bool {
    false
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
pub fn install() -> io::Result<()> {
    Err(io::Error::other("not supported on this OS"))
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
pub fn uninstall() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("/a b/it's"), r"'/a b/it'\''s'");
        assert_eq!(
            xml_escape(r#"a & <b> "c""#),
            "a &amp; &lt;b&gt; &quot;c&quot;"
        );
    }

    #[test]
    fn workflow_files_are_valid_plists_with_the_command() {
        let cmd = r#"open -n -a '/Applications/A & B.app' --args --quick "$@""#;
        let doc = workflow_document(cmd);
        assert!(
            doc.contains("open -n -a '/Applications/A &amp; B.app' --args --quick &quot;$@&quot;")
        );
        assert!(workflow_info_plist().contains(LABEL));
        // plutil (macOS) checks the XML is a well-formed property list.
        if Path::new("/usr/bin/plutil").exists() {
            for body in [doc, workflow_info_plist()] {
                let td = tempfile::tempdir().unwrap();
                let f = td.path().join("x.plist");
                std::fs::write(&f, body).unwrap();
                let out = std::process::Command::new("/usr/bin/plutil")
                    .arg("-lint")
                    .arg(&f)
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "{}",
                    String::from_utf8_lossy(&out.stdout)
                );
            }
        }
    }

    /// Automator itself runs the generated Quick Action, passing the files
    /// as arguments (a stand-in command records them).
    #[cfg(target_os = "macos")]
    #[test]
    fn automator_runs_the_quick_action() {
        // Hosted CI runners may not let Automator run workflows headless.
        if std::env::var_os("CI").is_some() {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        let wf = td.path().join(format!("{LABEL}.workflow"));
        std::fs::create_dir_all(wf.join("Contents")).unwrap();
        let out = td.path().join("args.txt");
        let cmd = format!(
            "printf '%s\\n' \"$@\" > {}",
            sh_quote(&out.to_string_lossy())
        );
        std::fs::write(wf.join("Contents/Info.plist"), workflow_info_plist()).unwrap();
        std::fs::write(wf.join("Contents/document.wflow"), workflow_document(&cmd)).unwrap();
        let a = td.path().join("a b.txt");
        std::fs::write(&a, "x").unwrap();
        let r = std::process::Command::new("/usr/bin/automator")
            .arg("-i")
            .arg(&a)
            .arg(&wf)
            .output()
            .unwrap();
        assert!(
            r.status.success(),
            "automator: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        let got = std::fs::read_to_string(&out).unwrap();
        assert_eq!(got.trim_end(), a.to_string_lossy());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn quick_action_launches_the_bundle_or_binary() {
        assert_eq!(
            quick_action_command(Path::new(
                "/Applications/Zip It.app/Contents/MacOS/zip-compressor"
            )),
            r#"open -n -a '/Applications/Zip It.app' --args --quick "$@""#
        );
        assert_eq!(
            quick_action_command(Path::new("/dev/target/debug/zip-compressor")),
            r#"'/dev/target/debug/zip-compressor' --quick "$@" >/dev/null 2>&1 &"#
        );
    }
}
