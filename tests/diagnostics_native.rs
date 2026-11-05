#![cfg(windows)]

use std::time::{Duration, Instant};

use lights_out::diagnostics::DiagnosticsWindow;
use lights_out::i18n::Translator;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::windows::{EventLoopBuilderExtWindows, WindowExtWindows};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::*;

#[test]
#[ignore = "requires an interactive Windows desktop; opens only the read-only diagnostic window"]
fn production_status_displays_updates_and_closes_without_starting_the_app() {
    let mut event_loop = EventLoopBuilder::new().with_any_thread(true).build();
    let mut view = DiagnosticsWindow::open(&event_loop, &Translator::new("en")).unwrap();
    let host = HWND(view.window.hwnd() as *mut _);
    let first = "LightsOut [dry-run]\r\nCPU: 38%\r\nblender.exe";
    let second = "LightsOut [dry-run]\r\nПростой: 00:47:00\r\nЗвук: проверка выключена";
    view.update("LightsOut - Status", first).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut phase = 0;
    let mut closed = false;
    event_loop.run_return(|event, _, flow| {
        if let Event::WindowEvent {
            window_id,
            event: WindowEvent::CloseRequested,
            ..
        } = &event
        {
            if *window_id == view.window.id() {
                closed = true;
            }
        }
        if matches!(event, Event::MainEventsCleared) && phase < 2 {
            unsafe {
                let edit = GetWindow(host, GW_CHILD).unwrap();
                assert!(IsWindowVisible(edit).as_bool());
                let style = GetWindowLongW(edit, GWL_STYLE) as u32;
                assert_ne!(style & ES_READONLY as u32, 0);
                assert_ne!(style & ES_MULTILINE as u32, 0);
                let mut class = [0u16; 32];
                let len = GetClassNameW(edit, &mut class);
                assert_eq!(String::from_utf16_lossy(&class[..len as usize]), "Edit");
                let mut text = vec![0u16; GetWindowTextLengthW(edit) as usize + 1];
                let len = GetWindowTextW(edit, &mut text);
                assert_eq!(
                    String::from_utf16_lossy(&text[..len as usize]),
                    if phase == 0 { first } else { second }
                );
                let mut host_rect = Default::default();
                let mut edit_rect = Default::default();
                GetClientRect(host, &mut host_rect).unwrap();
                GetClientRect(edit, &mut edit_rect).unwrap();
                assert!(edit_rect.right > 0 && edit_rect.right < host_rect.right);
                assert!(edit_rect.bottom > 0 && edit_rect.bottom < host_rect.bottom);
                if phase == 0 {
                    view.update("LightsOut - Состояние", second).unwrap();
                } else {
                    PostMessageW(Some(host), WM_CLOSE, WPARAM(0), LPARAM(0)).unwrap();
                }
                phase += 1;
            }
        }
        *flow = if closed || Instant::now() >= deadline {
            ControlFlow::Exit
        } else {
            ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(20))
        };
    });
    assert_eq!(phase, 2, "both language updates must render");
    assert!(closed, "native close must reach Tao");
    drop(view);
    drop(event_loop);
}
