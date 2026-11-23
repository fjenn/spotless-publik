// SPDX-License-Identifier: GPL-3.0-or-later
//! "Sign in with publik": what the app window shows instead of the app until
//! the person is signed in (Gate.svelte draws it). [`ensure_signed_in`] is the
//! flow, the same as publik's module, behind [`GateUi`] so it runs the same in
//! the app and in tests. [`spawn`] runs it at launch. Like publik's module
//! there is no way past it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Runtime};

use super::client::{self, Env, Wallet, UNREACHABLE};
use super::USES_AI;

// ---------------------------------------------------------------------------
// Screens (publik's words, unchanged)

pub const SIGN_IN_TITLE: &str = "Sign in with publik";
pub const SIGN_IN_BODY: &str = "This app uses your publik account. Click Sign in with publik, then sign in in your browser. This window continues when you are signed in.";
pub const SIGN_IN_WAITING: &str = "Waiting for you to sign in in your browser.";
pub const SIGN_IN_TIMED_OUT: &str =
    "No sign-in after 10 minutes. Click Sign in with publik to try again.";
pub const PLAN_TITLE: &str = "Choose how to pay for AI";
pub const PLAN_WAITING: &str = "Waiting for your plan. Finish in your browser.";
pub const PLAN_TIMED_OUT: &str = "No plan after 10 minutes. Click Get a plan to try again.";
pub const CONNECT_TITLE: &str = "Connect your plan";

/// One screen of the gate. Buttons are `(id, label)`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Screen {
    pub id: &'static str,
    pub title: String,
    pub body: String,
    pub note: Option<String>,
    pub buttons: Vec<(&'static str, &'static str)>,
}

/// What shows the gate: the window in the app, a script in tests.
pub trait GateUi {
    /// Shows `screen` until a button is pressed (its id) or `done` turns
    /// true (`None`).
    fn show(&mut self, screen: &Screen, done: Arc<AtomicBool>) -> Option<&'static str>;
    /// Opens a publik page in the person's browser.
    fn open_url(&mut self, url: &str);
}

pub fn sign_in_screen(note: Option<&str>) -> Screen {
    Screen {
        id: "sign_in",
        title: SIGN_IN_TITLE.into(),
        body: SIGN_IN_BODY.into(),
        note: note.map(str::to_string),
        buttons: vec![("sign_in", "Sign in with publik")],
    }
}

pub fn unreachable_screen(message: &str) -> Screen {
    Screen {
        id: "unreachable",
        title: SIGN_IN_TITLE.into(),
        body: if message.is_empty() {
            UNREACHABLE.into()
        } else {
            message.into()
        },
        note: None,
        buttons: vec![("retry", "Retry")],
    }
}

fn who(wallet: &Value) -> String {
    match wallet
        .pointer("/account/handle")
        .and_then(Value::as_str)
        .filter(|h| !h.is_empty())
    {
        Some(handle) => format!("You are signed in as @{handle}."),
        None => "You are signed in to publik.".into(),
    }
}

fn what(wallet: &Value) -> String {
    match wallet
        .pointer("/plan/label")
        .and_then(Value::as_str)
        .filter(|l| !l.is_empty())
    {
        Some(label) => format!("Your plan: {label}."),
        None => {
            let micros = wallet
                .get("balance_micros")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            format!("Your balance: ${:.2}.", micros as f64 / 1_000_000.0)
        }
    }
}

pub fn choose_screen(wallet: &Value, note: Option<&str>) -> Screen {
    if client::has_plan(wallet) {
        Screen {
            id: "choose",
            title: CONNECT_TITLE.into(),
            body: format!(
                "{} {} Connect it to use AI in this app, or use your own key.",
                who(wallet),
                what(wallet)
            ),
            note: note.map(str::to_string),
            buttons: vec![
                ("connect", "Connect your plan"),
                ("own_key", "Use my own key"),
            ],
        }
    } else {
        Screen {
            id: "choose",
            title: PLAN_TITLE.into(),
            body: format!(
                "{} This app uses AI. Get a publik plan, or use your own key.",
                who(wallet)
            ),
            note: note.map(str::to_string),
            buttons: vec![("get_plan", "Get a plan"), ("own_key", "Use my own key")],
        }
    }
}

/// Where "Sign in with publik" sends the person.
fn claim_url(env: &Env, wallet: &Value) -> String {
    let from = |v: &Value| {
        v.get("claim_url")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
            .map(str::to_string)
    };
    from(wallet)
        .or_else(|| from(&env.read_credential()))
        .unwrap_or_else(|| format!("{}/wallet", env.origin))
}

fn top_up_url(env: &Env, wallet: &Value) -> String {
    wallet
        .get("top_up_url")
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{}/wallet", env.origin))
}

// ---------------------------------------------------------------------------
// The gate

/// What the gate hands back once the person is through.
#[derive(Debug)]
pub struct Passed {
    pub key: String,
    pub wallet: Value,
    pub choice: Option<String>,
}

/// Looks at the wallet every `env.poll_every`, at most `env.polls` times,
/// until `check` holds; then sets `done`.
struct Poller {
    done: Arc<AtomicBool>,
    hit: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Poller {
    fn start(env: &Env, key: &str, check: fn(&Value) -> bool) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let hit = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (env, key) = (env.clone(), key.to_string());
        let (d, h, s) = (done.clone(), hit.clone(), stop.clone());
        std::thread::spawn(move || {
            for _ in 0..env.polls {
                std::thread::sleep(env.poll_every);
                if s.load(Ordering::SeqCst) {
                    return;
                }
                if let Ok(Wallet::Ok(wallet)) = client::read_wallet(&env, &key) {
                    if check(&wallet) {
                        h.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
            if !s.load(Ordering::SeqCst) {
                d.store(true, Ordering::SeqCst);
            }
        });
        Poller { done, hit, stop }
    }

    fn cancel(self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

enum Waited {
    /// `check` held.
    Done,
    /// A button other than the one that opens the browser.
    Pressed(&'static str),
}

/// Shows a screen whose `opener` button sends the person to `url`, and waits
/// for `check` to hold in the wallet: from the start, and again for ten
/// minutes after each press.
#[allow(clippy::too_many_arguments)]
fn wait_in_browser(
    ui: &mut dyn GateUi,
    env: &Env,
    key: &str,
    url: &str,
    screen: &dyn Fn(Option<&str>) -> Screen,
    opener: &str,
    waiting: &'static str,
    timed_out: &'static str,
    check: fn(&Value) -> bool,
) -> Waited {
    let mut note: Option<&'static str> = None;
    let mut poller = Some(Poller::start(env, key, check));
    loop {
        let done = poller
            .as_ref()
            .map(|p| p.done.clone())
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        match ui.show(&screen(note), done) {
            None => {
                let Some(p) = poller.take() else { continue };
                if p.hit.load(Ordering::SeqCst) {
                    return Waited::Done;
                }
                note = Some(timed_out);
            }
            Some(id) if id == opener => {
                ui.open_url(url);
                if let Some(p) = poller.take() {
                    p.cancel();
                }
                poller = Some(Poller::start(env, key, check));
                note = Some(waiting);
            }
            Some(id) => {
                if let Some(p) = poller.take() {
                    p.cancel();
                }
                return Waited::Pressed(id);
            }
        }
    }
}

/// Shows `screen` until a button is pressed.
fn ask(ui: &mut dyn GateUi, screen: &Screen) -> &'static str {
    loop {
        if let Some(id) = ui.show(screen, Arc::new(AtomicBool::new(false))) {
            return id;
        }
    }
}

/// publik's gate: a key for this computer (provisioning one if needed), a
/// signed-in publik account, and, for apps that use AI, the person's choice
/// of a publik plan or their own key. Returns only when all are there.
/// Usage counting starts after it ([`client::start_telemetry`]).
pub fn ensure_signed_in(env: &Env, ui: &mut dyn GateUi) -> Passed {
    let mut wiped = false;
    loop {
        let key = match env.key().map(Ok).unwrap_or_else(|| client::provision(env)) {
            Ok(key) => key,
            Err(message) => {
                ask(ui, &unreachable_screen(&message));
                continue;
            }
        };
        let wallet = match client::read_wallet(env, &key) {
            Ok(Wallet::Ok(wallet)) => wallet,
            // A stored key publik no longer knows: start this computer over.
            Ok(Wallet::Refused(401, _)) if env.api_key.is_none() && !wiped => {
                wiped = true;
                env.write_credential(&json!({}));
                continue;
            }
            Ok(Wallet::Refused(_, message)) => {
                ask(
                    ui,
                    &unreachable_screen(message.as_deref().unwrap_or(UNREACHABLE)),
                );
                continue;
            }
            Err(message) => {
                ask(ui, &unreachable_screen(&message));
                continue;
            }
        };

        if !client::is_claimed(&wallet) {
            let url = claim_url(env, &wallet);
            wait_in_browser(
                ui,
                env,
                &key,
                &url,
                &sign_in_screen,
                "sign_in",
                SIGN_IN_WAITING,
                SIGN_IN_TIMED_OUT,
                client::is_claimed,
            );
            continue;
        }

        let mut choice = env.choice();
        if USES_AI && choice.is_none() {
            if client::has_plan(&wallet) {
                match ask(ui, &choose_screen(&wallet, None)) {
                    "connect" => choice = Some("publik".into()),
                    "own_key" => choice = Some("own_key".into()),
                    _ => continue,
                }
                env.set_choice(choice.as_deref());
            } else {
                let url = top_up_url(env, &wallet);
                let screen = |note: Option<&str>| choose_screen(&wallet, note);
                if let Waited::Pressed("own_key") = wait_in_browser(
                    ui,
                    env,
                    &key,
                    &url,
                    &screen,
                    "get_plan",
                    PLAN_WAITING,
                    PLAN_TIMED_OUT,
                    client::has_plan,
                ) {
                    env.set_choice(Some("own_key"));
                }
                // With a plan now, the next round asks to connect it.
                continue;
            }
        }
        return Passed {
            key,
            wallet,
            choice,
        };
    }
}

// ---------------------------------------------------------------------------
// The screen in the app window

static STARTED: AtomicBool = AtomicBool::new(false);
static PASSED: AtomicBool = AtomicBool::new(false);
/// Set once the gate sent the person to publik in the browser.
static SENT_TO_BROWSER: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct Shared {
    screen: Mutex<Option<Screen>>,
    presses: Mutex<Option<mpsc::Sender<String>>>,
}

fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(Shared::default)
}

/// What the app window shows: the gate's current screen, or `None` once the
/// person is signed in (then the app shows itself). Polled by Gate.svelte.
#[tauri::command]
pub fn publik_gate_screen() -> Option<Screen> {
    if PASSED.load(Ordering::SeqCst) {
        return None;
    }
    Some(
        shared()
            .screen
            .lock()
            .ok()
            .and_then(|s| s.clone())
            .unwrap_or_else(|| waiting_screen()),
    )
}

/// A button on the sign-in screen was pressed.
#[tauri::command]
pub fn publik_gate_press(id: String) {
    if let Some(tx) = shared().presses.lock().ok().and_then(|t| t.clone()) {
        let _ = tx.send(id);
    }
}

/// Before the first answer from publik: the sign-in screen without a button.
fn waiting_screen() -> Screen {
    let mut s = sign_in_screen(Some(SIGN_IN_WAITING));
    s.buttons.clear();
    s
}

/// The gate's screens, shown by the app window (Gate.svelte asks for them).
struct WindowUi {
    presses: mpsc::Receiver<String>,
}

impl GateUi for WindowUi {
    fn show(&mut self, screen: &Screen, done: Arc<AtomicBool>) -> Option<&'static str> {
        if let Ok(mut current) = shared().screen.lock() {
            *current = Some(screen.clone());
        }
        // A press meant for an earlier screen does not count.
        while self.presses.try_recv().is_ok() {}
        loop {
            if done.load(Ordering::SeqCst) {
                return None;
            }
            match self.presses.recv_timeout(Duration::from_millis(100)) {
                Ok(id) => {
                    if let Some((id, _)) = screen.buttons.iter().find(|(b, _)| *b == id) {
                        return Some(id);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    }

    fn open_url(&mut self, url: &str) {
        if !url.starts_with("https://publikhq.com/") {
            log::warn!("publik: not opening an unexpected address");
            return;
        }
        SENT_TO_BROWSER.store(true, Ordering::SeqCst);
        #[cfg(target_os = "macos")]
        let r = std::process::Command::new("open").arg(url).status();
        #[cfg(target_os = "windows")]
        // Not `cmd /C start`: cmd would split the address at an `&`.
        let r = client::quiet_command("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .status();
        #[cfg(all(unix, not(target_os = "macos")))]
        let r = std::process::Command::new("xdg-open").arg(url).status();
        if let Err(e) = r {
            log::warn!("publik: could not open the browser: {e}");
        }
    }
}

/// Called once from the app's setup: runs the gate on its own thread. The app
/// window shows the sign-in screen (Gate.svelte) until it passes; then usage
/// counting starts and `publik-signed-in` tells the window to show the app.
pub fn spawn<R: Runtime>(app: &AppHandle<R>) {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let (tx, rx) = mpsc::channel();
    if let Ok(mut presses) = shared().presses.lock() {
        *presses = Some(tx);
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let mut ui = WindowUi { presses: rx };
        let passed = ensure_signed_in(&Env::from_process(), &mut ui);
        log::info!(
            "publik: signed in (choice {:?})",
            passed.choice.as_deref().unwrap_or("none")
        );
        PASSED.store(true, Ordering::SeqCst);
        client::start_telemetry();
        use tauri::Emitter;
        let _ = app.emit("publik-signed-in", ());
        if SENT_TO_BROWSER.load(Ordering::SeqCst) {
            bring_to_front(&app);
        }
    });
}

/// After a sign-in in the browser, put Spotless back in front, centred.
fn bring_to_front<R: Runtime>(app: &AppHandle<R>) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        use tauri::Manager;
        #[cfg(target_os = "macos")]
        let _ = handle.show();
        let window = handle
            .get_webview_window("main")
            .or_else(|| handle.webview_windows().into_values().next());
        if let Some(w) = window {
            let _ = w.unminimize();
            let _ = w.show();
            let _ = w.center();
            let _ = w.set_focus();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{self, FakePublik};
    use std::collections::VecDeque;

    /// Presses buttons from a script; waits out `done` when told to.
    struct Script {
        steps: VecDeque<Option<&'static str>>,
        seen: Vec<Screen>,
        opened: Vec<String>,
    }

    impl Script {
        fn new(steps: &[Option<&'static str>]) -> Self {
            Script {
                steps: steps.iter().copied().collect(),
                seen: vec![],
                opened: vec![],
            }
        }
    }

    impl GateUi for Script {
        fn show(&mut self, screen: &Screen, done: Arc<AtomicBool>) -> Option<&'static str> {
            self.seen.push(screen.clone());
            match self
                .steps
                .pop_front()
                .expect("the gate asked for more than the script has")
            {
                Some(id) => Some(id),
                None => {
                    let start = std::time::Instant::now();
                    while !done.load(Ordering::SeqCst) {
                        assert!(start.elapsed() < Duration::from_secs(10), "never done");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    None
                }
            }
        }
        fn open_url(&mut self, url: &str) {
            self.opened.push(url.to_string());
        }
    }

    #[test]
    fn a_new_computer_signs_in_through_the_browser() {
        let publik = FakePublik::start();
        let env = fake::env(&publik);
        // Signed in on publik's site after the person clicks.
        publik.claim_after_wallet_reads(3);
        let mut ui = Script::new(&[Some("sign_in"), None]);
        let passed = ensure_signed_in(&env, &mut ui);
        assert!(passed.key.starts_with("pk_"));
        assert!(client::is_claimed(&passed.wallet));
        assert_eq!(ui.seen[0], sign_in_screen(None));
        assert_eq!(ui.seen[1], sign_in_screen(Some(SIGN_IN_WAITING)));
        assert_eq!(ui.opened, vec![format!("{}/claim/abc", publik.origin())]);
        // The key is kept for the next launch.
        assert_eq!(env.key().as_deref(), Some(passed.key.as_str()));
        assert_eq!(publik.installs(), 1);
    }

    #[test]
    fn a_signed_in_computer_goes_straight_through() {
        let publik = FakePublik::start();
        let env = fake::env(&publik);
        publik.claim_after_wallet_reads(0);
        let mut ui = Script::new(&[]);
        ensure_signed_in(&env, &mut ui);
        ensure_signed_in(&env, &mut ui);
        assert!(ui.seen.is_empty());
        assert_eq!(publik.installs(), 1);
    }

    #[test]
    fn no_sign_in_for_ten_minutes_says_so() {
        let publik = FakePublik::start();
        let env = fake::env(&publik);
        publik.claim_after_wallet_reads(u32::MAX);
        let mut ui = Script::new(&[None]);
        let stop = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ensure_signed_in(&env, &mut ui)
        }));
        assert!(stop.is_err(), "the script ran out, as planned");
        assert_eq!(
            ui.seen.last(),
            Some(&sign_in_screen(Some(SIGN_IN_TIMED_OUT)))
        );
    }

    #[test]
    fn publik_unreachable_offers_retry() {
        let publik = FakePublik::start();
        let mut env = fake::env(&publik);
        env.origin = "http://127.0.0.1:9".into();
        let mut ui = Script::new(&[Some("retry")]);
        let stop = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ensure_signed_in(&env, &mut ui)
        }));
        assert!(stop.is_err());
        assert_eq!(ui.seen[0], unreachable_screen(UNREACHABLE));
        assert_eq!(ui.seen.len(), 2);
    }

    #[test]
    fn a_build_without_a_token_says_so() {
        let publik = FakePublik::start();
        let mut env = fake::env(&publik);
        env.token = None;
        let mut ui = Script::new(&[Some("retry")]);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ensure_signed_in(&env, &mut ui)
        }));
        assert_eq!(ui.seen[0].body, client::NO_TOKEN);
    }

    #[test]
    fn a_forgotten_key_starts_the_computer_over() {
        let publik = FakePublik::start();
        let env = fake::env(&publik);
        env.write_credential(&json!({"key": "pk_old", "install_id": "x"}));
        publik.claim_after_wallet_reads(0);
        let passed = ensure_signed_in(&env, &mut Script::new(&[]));
        assert_ne!(passed.key, "pk_old");
        assert_eq!(publik.installs(), 1);
    }

    #[test]
    fn the_choose_screens_use_publiks_words() {
        let none = json!({"claim_state": "claimed", "account": {"handle": "finn"}});
        let s = choose_screen(&none, None);
        assert_eq!(s.title, PLAN_TITLE);
        assert_eq!(
            s.body,
            "You are signed in as @finn. This app uses AI. Get a publik plan, or use your own key."
        );
        let plan = json!({"claim_state": "claimed", "plan": {"kind": "pro", "label": "Pro"}});
        let s = choose_screen(&plan, None);
        assert_eq!(s.title, CONNECT_TITLE);
        assert_eq!(
            s.body,
            "You are signed in to publik. Your plan: Pro. Connect it to use AI in this app, or use your own key."
        );
        let credit = json!({"plan": {"kind": "none"}, "balance_micros": 2_500_000});
        assert!(choose_screen(&credit, None)
            .body
            .contains("Your balance: $2.50."));
    }

    #[test]
    fn the_window_shows_the_gate_until_signed_in() {
        // Before the gate's first screen the window shows the waiting screen.
        let s = publik_gate_screen().expect("a screen before sign-in");
        assert_eq!(s.title, SIGN_IN_TITLE);
        assert!(s.buttons.is_empty());
    }
}
