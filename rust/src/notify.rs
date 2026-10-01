//! A desktop notification, sent without waiting: a Windows toast (through a hidden PowerShell,
//! which can reach the WinRT toast API), `osascript` on macOS, `notify-send` on Linux. The text
//! travels in environment variables, so nothing in it is ever parsed as code.
//! REMAN_NOTIFY_LOG=<file> writes the notification there instead (tests). A daemon on a sandbox
//! database (REMAN_DB set: the test suites, a trial run) never shows one on the desktop.

pub fn send(title: &str, body: &str) {
    if let Some(log) = std::env::var_os("REMAN_NOTIFY_LOG") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
            let _ = writeln!(f, "{title}\t{body}");
        }
        return;
    }
    if std::env::var_os("REMAN_DB").is_some() {
        return;
    }
    let mut cmd = command();
    cmd.env("REMAN_N_TITLE", title).env("REMAN_N_BODY", body).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    if let Ok(mut child) = cmd.spawn() {
        // reaped in the background, so it never lingers as a zombie
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

#[cfg(windows)]
fn command() -> std::process::Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // Windows PowerShell's own app id: a toast needs a registered one
    const SCRIPT: &str = r#"
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null
$x = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02)
$t = $x.GetElementsByTagName('text')
$t.Item(0).AppendChild($x.CreateTextNode($env:REMAN_N_TITLE)) > $null
$t.Item(1).AppendChild($x.CreateTextNode($env:REMAN_N_BODY)) > $null
$id = '{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe'
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($id).Show([Windows.UI.Notifications.ToastNotification]::new($x))
"#;
    let mut c = std::process::Command::new("powershell.exe");
    c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", SCRIPT]).creation_flags(CREATE_NO_WINDOW);
    c
}

#[cfg(target_os = "macos")]
fn command() -> std::process::Command {
    let mut c = std::process::Command::new("osascript");
    c.args(["-e", r#"display notification (system attribute "REMAN_N_BODY") with title (system attribute "REMAN_N_TITLE")"#]);
    c
}

#[cfg(all(unix, not(target_os = "macos")))]
fn command() -> std::process::Command {
    // notify-send takes the text as arguments: through sh, from the environment
    let mut c = std::process::Command::new("sh");
    c.args(["-c", r#"notify-send -a reman "$REMAN_N_TITLE" "$REMAN_N_BODY""#]);
    c
}
