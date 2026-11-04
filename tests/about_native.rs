#![cfg(windows)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lights_out::{about::AboutWindow, config::Paths, i18n::Translator, version};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::windows::{EventLoopBuilderExtWindows, WindowExtWindows};

struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        for _ in 0..10 {
            if std::fs::remove_dir_all(&self.0).is_ok() || !self.0.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        eprintln!("Could not remove smoke scratch: {}", self.0.display());
    }
}

struct Probe(Child);

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// UIA runs out of process so a stalled accessibility provider cannot hang the GUI loop.
const PROBE: &str = r#"
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class NativeAboutProbe {
    [DllImport("user32.dll", EntryPoint="GetWindowLongW")]
    public static extern int GetWindowLong(IntPtr hwnd, int index);
    [DllImport("user32.dll", EntryPoint="SendMessageW")]
    public static extern IntPtr SendMessage(IntPtr hwnd, uint message, IntPtr wparam, IntPtr lparam);
}
'@
$deadline = [DateTime]::UtcNow.AddSeconds(10)
$last = ''
while ([DateTime]::UtcNow -lt $deadline) {
    try {
        $root = [System.Windows.Automation.AutomationElement]::FromHandle([IntPtr]::new([long]$env:ABOUT_SMOKE_HWND))
        if ($root.Current.ProcessId -ne [int]$env:ABOUT_SMOKE_PID) { throw 'Wrong window owner' }
        $nodes = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
        $repository = $false
        $repoLink = $false
        $releasesLink = $false
        $version = $false
        $config = $false
        $logs = $false
        $close = $false
        $closeHandle = [IntPtr]::Zero
        $status = $false
        $names = @()
        foreach ($node in $nodes) {
            $current = $node.Current
            if ($current.IsOffscreen -or $current.BoundingRectangle.Width -le 0 -or $current.BoundingRectangle.Height -le 0) { continue }
            $names += ($current.Name + ' [' + $current.ClassName + '/' + $current.ControlType.ProgrammaticName + ']')
            if ($current.ClassName -match 'Chrome|WebView') { throw 'Unexpected WebView child in native About' }
            if ($current.ClassName -ieq 'Static') {
                if ($current.Name -ceq 'Repository') { $repository = $true }
                if ($current.Name -ceq ('Version: ' + $env:ABOUT_SMOKE_VERSION)) { $version = $true }
                if ($current.Name -ceq ('Config: ' + $env:ABOUT_SMOKE_CONFIG)) { $config = $true }
                if ($current.Name -ceq ('Logs: ' + $env:ABOUT_SMOKE_LOGS)) { $logs = $true }
                if ($current.Name -match 'Checking for updates|latest stable version|New version available|Could not check for updates') { $status = $true }
            }
            # Classic Win32 providers can expose Button HWNDs as Pane on this desktop.
            # Verify the real native class and keyboard style, not an assumed UIA type.
            if ($current.ClassName -ieq 'Button') {
                $style = [NativeAboutProbe]::GetWindowLong([IntPtr]::new($current.NativeWindowHandle), -16)
                if (($style -band 0x10000) -eq 0 -or ($style -band 0x10000000) -eq 0) { throw 'Native button has no WS_TABSTOP/WS_VISIBLE' }
                if ($current.Name -ceq $env:ABOUT_SMOKE_REPO) { $repoLink = $true }
                if ($current.Name -ceq ($env:ABOUT_SMOKE_REPO + '/releases')) { $releasesLink = $true }
                if ($current.Name -ceq 'Close') { $close = $true; $closeHandle = [IntPtr]::new($current.NativeWindowHandle) }
            }
        }
        $last = $names -join ' | '
        if ($repository -and $repoLink -and $releasesLink -and $version -and $config -and $logs -and $close -and $status) {
            'Production About rendered native Static labels, update status, and keyboard-accessible Button links/Close; no WebView child'
            # Only click Close; URL buttons must never launch a browser in this test.
            [void][NativeAboutProbe]::SendMessage($closeHandle, 0x00F5, [IntPtr]::Zero, [IntPtr]::Zero)
            exit 0
        }
    } catch { $last = $_.Exception.Message }
    Start-Sleep -Milliseconds 100
}
[Console]::Error.WriteLine('About was blank or incomplete. Visible UIA names: ' + $last)
exit 1
"#;

#[test]
#[ignore = "requires an interactive Windows desktop; cargo test --test about_native -- --ignored --nocapture"]
fn production_about_renders_native_accessible_links() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();
    let deadline = Instant::now() + Duration::from_secs(14);
    let scratch = Scratch(std::env::temp_dir().join(format!(
        "lights-out-about-smoke-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    )));
    std::fs::create_dir(&scratch.0).unwrap();
    let paths = Paths {
        config_file: scratch.0.join("config.toml"),
        log_dir: scratch.0.join("logs"),
        webview_dir: scratch.0.join("webview"),
    };
    let mut event_loop = EventLoopBuilder::new().with_any_thread(true).build();
    let about = AboutWindow::open(&event_loop, &Translator::new("en"), &paths, true)
        .expect("construct production native About without starting the application");
    let mut probe = Probe(
        Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-STA",
                "-Command",
                PROBE,
            ])
            .env("ABOUT_SMOKE_HWND", about.window.hwnd().to_string())
            .env("ABOUT_SMOKE_PID", std::process::id().to_string())
            .env("ABOUT_SMOKE_REPO", version::REPO)
            .env("ABOUT_SMOKE_VERSION", version::VERSION)
            .env("ABOUT_SMOKE_CONFIG", &paths.config_file)
            .env("ABOUT_SMOKE_LOGS", &paths.log_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start isolated native UIAutomation probe"),
    );
    let mut finished = None;
    let mut close_requested = false;
    event_loop.run_return(|event, _, flow| {
        if let Event::WindowEvent {
            window_id,
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            if window_id == about.window.id() {
                close_requested = true;
            }
        }
        finished = probe.0.try_wait().transpose();
        *flow = if (finished.is_some() && close_requested) || Instant::now() >= deadline {
            ControlFlow::Exit
        } else {
            ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(40))
        };
    });
    let _ = probe.0.kill();
    let _ = probe.0.wait();
    use std::io::Read;
    let mut stdout = String::new();
    let mut stderr = String::new();
    probe
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    probe
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    drop(probe);
    drop(about);
    drop(event_loop);
    assert!(
        !paths.webview_dir.exists(),
        "native About created a WebView profile"
    );
    assert!(
        close_requested,
        "native Close button did not reach Tao CloseRequested"
    );
    drop(scratch);
    assert!(
        matches!(finished, Some(Ok(status)) if status.success()),
        "native About UIA probe failed or exceeded 14s: {finished:?}\n{stdout}\n{stderr}"
    );
    println!("{stdout}");
}
