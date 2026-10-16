//! #105 "Show me what you're sending": the bridge between the worker thread
//! that is about to send a request and the main thread that owns the card.
//!
//! The worker (`provider::Chain`, through [`WindowSendGate`]) posts a
//! [`SendRequest`] to the owner window and then blocks on a channel. The main
//! thread shows the request as a preview card (`Card::show_send_preview`) and
//! keeps the channel's sender in its one pending-preview slot
//! (`App::pending_preview`, `PendingPreview::Send`). [`decide`] is the only
//! thing that turns a press of "Send" into a [`SendAuthorized`]; Cancel, Esc,
//! click-away and every other way the preview can disappear drop the sender,
//! which wakes the worker with no authorization. Nothing here polls: the
//! worker sleeps in `recv` until the main thread answers or drops the sender.

use std::sync::mpsc::{channel, Sender};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::app::WM_APP_SEND_PREVIEW;
use crate::provider::{Request, SendGate};
use crate::ui::confirm::{authorize_send, user_confirmed_send, SendAuthorized};
use crate::ui::preview::RequestPreview;

/// What the worker posts to the owner window. Boxed into `lparam` by
/// [`WindowSendGate`]; the receiver takes ownership.
pub struct SendRequest {
    /// The provider this request is about to go to, shown in the title.
    pub provider_id: String,
    /// What is about to be sent, as it will actually be sent.
    pub preview: RequestPreview,
    /// Answered by [`decide`] on Send, dropped on anything else.
    pub reply: Sender<SendAuthorized>,
}

/// The preview card's title. Plain words, no em dash (rule 11).
pub fn title_for(provider_id: &str) -> String {
    format!("Send to {provider_id}?")
}

/// The main thread's answer. `pressed_send` is true only when the card's
/// confirm button (or Enter) closed the preview; this is the one call site of
/// `user_confirmed_send`, so no other outcome can authorize a send. On
/// `false` the sender is simply dropped, which is how the worker learns the
/// preview was cancelled or abandoned.
pub fn decide(reply: Sender<SendAuthorized>, pressed_send: bool) {
    if pressed_send {
        // A closed receiver means the worker is gone; nothing to do.
        let _ = reply.send(authorize_send(user_confirmed_send()));
    }
}

/// The worker's half, with the transport to the main thread injected so it
/// can be tested without a window. `post` hands the request over and returns
/// `false` if it could not (the window is gone): then nothing is sent.
pub fn ask(
    post: impl FnOnce(SendRequest) -> bool,
    provider_id: &str,
    req: &Request,
) -> Option<SendAuthorized> {
    let (reply, rx) = channel();
    let request = SendRequest {
        provider_id: provider_id.to_string(),
        preview: RequestPreview::from_request(req),
        reply,
    };
    if !post(request) {
        return None;
    }
    // Blocks until the main thread answers or drops the sender. Every path
    // that removes the preview drops it, so this cannot wait forever.
    rx.recv().ok()
}

thread_local! {
    /// #723: the run generation this worker thread was started for. Set by
    /// each worker on its first line; read when it posts a preview, so a run
    /// the app gave up on cannot put a preview over a newer one. `0` (never
    /// set) is always accepted.
    static WORKER_RUN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Records the run generation for this worker thread (#723).
pub fn set_worker_run(run: u64) {
    WORKER_RUN.with(|c| c.set(run));
}

/// The run generation [`set_worker_run`] recorded for this thread (`0` if never
/// set): what a worker that hands a result to the main thread by side channel
/// keys it with (#809).
pub fn worker_run() -> u64 {
    WORKER_RUN.with(|c| c.get())
}

/// The real gate: posts to the owner window. `hwnd` is carried as an `isize`
/// because an `HWND` wraps a raw pointer and is not `Send`.
pub struct WindowSendGate {
    hwnd: isize,
}

impl WindowSendGate {
    pub fn new(hwnd: isize) -> Self {
        Self { hwnd }
    }
}

impl SendGate for WindowSendGate {
    fn confirm(&self, provider_id: &str, req: &Request) -> Option<SendAuthorized> {
        let hwnd = self.hwnd;
        let run = WORKER_RUN.with(|c| c.get());
        ask(
            move |request| {
                let raw = Box::into_raw(Box::new(request));
                let posted = unsafe {
                    PostMessageW(
                        Some(HWND(hwnd as *mut _)),
                        WM_APP_SEND_PREVIEW,
                        WPARAM(run as usize),
                        LPARAM(raw as isize),
                    )
                };
                if posted.is_err() {
                    // Never delivered, so the box is still ours to free.
                    drop(unsafe { Box::from_raw(raw) });
                    return false;
                }
                true
            },
            provider_id,
            req,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Effort;

    fn request() -> Request {
        Request {
            system: "sys".to_string(),
            user: "hello".to_string(),
            images: vec![vec![1, 2, 3]],
            schema: None,
            effort: Effort::Unset,
            max_tokens: 0,
        }
    }

    #[test]
    fn send_hands_the_worker_an_authorization() {
        let auth = ask(
            |r| {
                decide(r.reply, true);
                true
            },
            "anthropic",
            &request(),
        );
        assert!(auth.is_some());
    }

    #[test]
    fn cancel_hands_the_worker_nothing() {
        let auth = ask(
            |r| {
                decide(r.reply, false);
                true
            },
            "anthropic",
            &request(),
        );
        assert!(auth.is_none());
    }

    #[test]
    fn an_abandoned_preview_hands_the_worker_nothing() {
        // The main thread drops the request (the preview was replaced, the
        // card hidden, Settings was open): the sender goes with it.
        let auth = ask(
            |r| {
                drop(r);
                true
            },
            "anthropic",
            &request(),
        );
        assert!(auth.is_none());
    }

    #[test]
    fn an_undeliverable_request_sends_nothing() {
        let auth = ask(|_| false, "anthropic", &request());
        assert!(auth.is_none());
    }

    #[test]
    fn the_request_carries_the_real_text_and_the_destination() {
        let mut seen = None;
        let _ = ask(
            |r| {
                seen = Some((r.provider_id.clone(), r.preview.clone()));
                true
            },
            "openai",
            &request(),
        );
        let (id, preview) = seen.expect("post was called");
        assert_eq!(id, "openai");
        assert_eq!(preview.user_text, "hello");
        assert_eq!(preview.images[0].size_bytes, 3);
    }

    #[test]
    fn the_title_names_the_destination_without_an_em_dash() {
        let title = title_for("anthropic");
        assert!(title.contains("anthropic"));
        assert!(!title.contains('\u{2014}'));
    }

    /// The real transport: a worker thread calls the window gate, the
    /// message lands in the window's queue with the boxed payload, and the
    /// answer given here wakes the worker. If `WindowSendGate` posted to the
    /// wrong id or the wrong window, the receive below would time out.
    #[test]
    fn the_window_gate_posts_the_request_and_the_answer_wakes_the_worker() {
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, PeekMessageW, MSG, PM_REMOVE, WINDOW_EX_STYLE,
            WS_OVERLAPPED,
        };

        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("send gate test"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )
        }
        .expect("create a test window on this thread");

        let gate = WindowSendGate::new(hwnd.0 as isize);
        let worker = std::thread::spawn(move || gate.confirm("anthropic", &request()));

        let started = std::time::Instant::now();
        let request = loop {
            let mut msg = MSG::default();
            let got = unsafe {
                PeekMessageW(
                    &mut msg,
                    Some(hwnd),
                    WM_APP_SEND_PREVIEW,
                    WM_APP_SEND_PREVIEW,
                    PM_REMOVE,
                )
            };
            if got.as_bool() {
                break unsafe { *Box::from_raw(msg.lParam.0 as *mut SendRequest) };
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "the worker never posted WM_APP_SEND_PREVIEW"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(request.provider_id, "anthropic");
        decide(request.reply, true);

        let auth = worker.join().expect("worker thread");
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        assert!(auth.is_some());
    }

    #[test]
    fn a_decision_arrives_across_threads() {
        // The real shape: the worker blocks, another thread answers.
        let auth = ask(
            |r| {
                std::thread::spawn(move || decide(r.reply, true));
                true
            },
            "anthropic",
            &request(),
        );
        assert!(auth.is_some());
    }
}
