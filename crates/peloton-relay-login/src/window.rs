//! Window mode: log in inside an incognito webview and capture the callback.

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
    callback.ok_or(LoginError::Cancelled)
}
