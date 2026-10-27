//! Window mode: log in inside an incognito webview and capture the callback.

use muda::{Menu, PredefinedMenuItem, Submenu};
use peloton::oauth::REDIRECT_URI;
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn as _;
use tao::window::WindowBuilder;
use url::Url;
use wry::WebViewBuilder;

use crate::LoginError;

/// Opens a window on `authorize_url` and returns the callback URL once the
/// login redirects to Peloton's redirect URI. Must run on the main thread.
///
/// # Errors
///
/// Returns [`LoginError::Cancelled`] when the window is closed first, and
/// [`LoginError::Window`] when the window or webview cannot be created.
pub fn capture_callback(authorize_url: &Url) -> Result<String, LoginError> {
    let mut event_loop = EventLoopBuilder::<String>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    // Kept alive until the loop ends: macOS sends Cmd+V and friends through it.
    let menu = edit_menu()?;
    menu.init_for_nsapp();
    let window = WindowBuilder::new()
        .with_title("Peloton login")
        .with_inner_size(LogicalSize::new(480.0, 720.0))
        .build(&event_loop)
        .map_err(|e| LoginError::Window(e.to_string()))?;
    let webview = WebViewBuilder::new()
        .with_incognito(true)
        .with_url(authorize_url.as_str())
        .with_navigation_handler(move |url| {
            if url.starts_with(REDIRECT_URI) {
                // A failed send means the loop is already gone; cancelling the
                // navigation is still right.
                let _ = proxy.send_event(url);
                false
            } else {
                true
            }
        })
        .build(&window)
        .map_err(|e| LoginError::Window(e.to_string()))?;

    let mut callback = None;
    event_loop.run_return(|event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(url) => {
                callback = Some(url);
                *control_flow = ControlFlow::Exit;
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            _ => {}
        }
    });
    drop(webview);
    drop(window);
    menu.remove_for_nsapp();
    callback.ok_or(LoginError::Cancelled)
}

/// The app menu with Close Window, and an Edit menu so the standard editing
/// shortcuts (Cmd+C, Cmd+V, Cmd+X, Cmd+A, Cmd+Z) reach the webview.
fn edit_menu() -> Result<Menu, LoginError> {
    let window_error = |e: muda::Error| LoginError::Window(e.to_string());
    let app = Submenu::with_items(
        "Peloton login",
        true,
        &[&PredefinedMenuItem::close_window(None)],
    )
    .map_err(window_error)?;
    let edit = Submenu::with_items(
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(None),
            &PredefinedMenuItem::redo(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::cut(None),
            &PredefinedMenuItem::copy(None),
            &PredefinedMenuItem::paste(None),
            &PredefinedMenuItem::select_all(None),
        ],
    )
    .map_err(window_error)?;
    Menu::with_items(&[&app, &edit]).map_err(window_error)
}
