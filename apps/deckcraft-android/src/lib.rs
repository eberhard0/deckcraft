//! DeckCraft on Android.
//!
//! Runs the same [`deckcraft_ui_egui::SlideApp`] as the desktop app inside a `GameActivity`
//! (android-activity's `game-activity` backend, which eframe needs for the soft keyboard and
//! accesskit). Built with `cargo ndk` into `android/app/src/main/jniLibs`, then packaged by the
//! Gradle project in `android/`.
//!
//! Differences from the desktop app (it mirrors the web shell, `apps/deckcraft-web`):
//! - no TCP control server;
//! - File › Open and Insert › Pictures/Audio/Video ask `MainActivity.pickOpen()` (Storage Access
//!   Framework); the bytes come back on a Java thread through `nativeDeliverFile` into
//!   `Services::inbox` (presentations open, media is inserted);
//! - Save, Save As and Export write to `Downloads/DeckCraft/<name>` through
//!   `MainActivity.saveToDownloads` (MediaStore, no dialog) via `Services::download`, and saving
//!   the same name again overwrites that file;
//! - no audio output (cpal is desktop-only): media plays silently, as with `DECKCRAFT_NO_AUDIO`;
//! - no clipboard images;
//! - the UI state (`ui.json`), preferences (`prefs.json`) and the AutoRecover folder live in the
//!   app's private files directory;
//! - links open through `MainActivity.openUrl`.

#![cfg(target_os = "android")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

use android_activity::AndroidApp;
use deckcraft_engine::Session;
use deckcraft_ui_egui::{Inbox, Services, SlideApp, UiState};
use jni::objects::{JByteArray, JObject, JString};
use jni::{Env, EnvUnowned, JavaVM, jni_sig, jni_str};

const LOG_TAG: &str = "deckcraft";

/// Files the Kotlin side delivers (name, bytes); the app opens them on its next frame.
static INBOX: OnceLock<Inbox> = OnceLock::new();
/// The egui context, to wake the app when a file arrives from a Java thread.
static CTX: OnceLock<egui::Context> = OnceLock::new();
/// The process's Java VM (set once) and the current activity (a reference android-activity
/// owns, stored as an address; 0 = none).
static VM: OnceLock<JavaVM> = OnceLock::new();
static ACTIVITY: Mutex<usize> = Mutex::new(0);

fn inbox() -> &'static Inbox {
    INBOX.get_or_init(Inbox::default)
}

/// The activity's entry point, called by android-activity's GameActivity glue on its own thread.
/// It returns when the activity is destroyed.
#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    static LOGGER: OnceLock<()> = OnceLock::new();
    LOGGER.get_or_init(|| {
        android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag(LOG_TAG));
    });
    // SAFETY: `vm_as_ptr` is the process's JavaVM, valid for the life of the process.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let _ = VM.set(vm);
    *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner) = app.activity_as_ptr() as usize;

    let data_dir = app.internal_data_path().unwrap_or_else(|| PathBuf::from("/data/local/tmp"));
    log::info!("DeckCraft {} starting; data in {}", env!("CARGO_PKG_VERSION"), data_dir.display());
    let options = eframe::NativeOptions {
        android_app: Some(app),
        // eframe saves egui panel/window sizes here on exit.
        persistence_path: Some(data_dir.join("ui.ron")),
        ..Default::default()
    };
    let result = eframe::run_native(
        "DeckCraft",
        options,
        Box::new(move |cc| {
            if let Some(rs) = &cc.wgpu_render_state {
                let info = rs.adapter.get_info();
                log::info!("wgpu backend {:?}, adapter {}", info.backend, info.name);
            }
            let _ = CTX.set(cc.egui_ctx.clone());
            let mut app = SlideApp::new(Session::new(), services());
            let prefs = data_dir.join("ui.json");
            load_prefs(&mut app, &prefs);
            // AutoRecover: reopen what a previous run left unsaved, then keep it current.
            app.session.recovery_dir = Some(data_dir.join("recovery"));
            if let Ok(v) = app.session.execute("file.recovery.open", &serde_json::json!({}))
                && let Some(n) = v.get("opened").and_then(|o| o.as_array()).map(Vec::len).filter(|n| *n > 0)
            {
                app.set_status(format!("Recovered {n} unsaved presentation{} from the last session", if n == 1 { "" } else { "s" }));
            }
            let last_prefs = prefs_text(&app);
            Ok(Box::new(AndroidShell { app, prefs, last_prefs, last_prefs_check: 0.0 }))
        }),
    );
    *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner) = 0;
    if let Err(e) = &result {
        log::error!("DeckCraft stopped: {e}");
    }
    // winit allows one event loop per process: when Android recreates the activity in the same
    // process, end the process so the next launch starts clean instead of a blank window.
    std::process::exit(0);
}

/// Wraps the app for eframe: preferences, and links handed to the activity.
struct AndroidShell {
    app: SlideApp,
    prefs: PathBuf,
    last_prefs: String,
    last_prefs_check: f64,
}

impl eframe::App for AndroidShell {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.app.logic(ctx);
        // Android rarely calls `on_exit`: persist the UI state a few seconds after it changes.
        let now = ctx.input(|i| i.time);
        if now - self.last_prefs_check > 5.0 {
            self.last_prefs_check = now;
            save_prefs(&self.app, &self.prefs, &mut self.last_prefs);
        }
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.app.raw_input_hook(raw);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
        // eframe has no browser on Android: open links through the activity instead.
        ui.ctx().output_mut(|o| {
            o.commands.retain(|c| match c {
                egui::OutputCommand::OpenUrl(u) => {
                    if let Err(e) = open_url(&u.url) {
                        log::error!("couldn't open {}: {e}", u.url);
                    }
                    false
                }
                _ => true,
            })
        });
    }

    fn on_exit(&mut self) {
        save_prefs(&self.app, &self.prefs, &mut self.last_prefs);
    }
}

/// The web shell's services with Android implementations: no file paths, so `pick_open`,
/// `pick_save` and `read` stay `None` and the UI falls back to `open_async` (system picker →
/// inbox) and `download` (Downloads/DeckCraft). `write` covers the control/export paths that
/// name a file: the last path component becomes the name in Downloads.
fn services() -> Services {
    Services {
        // The picker is asynchronous: the file arrives later through the inbox.
        open_async: Some(Box::new(|_purpose: &str| {
            if let Err(e) = pick_open() {
                log::error!("couldn't open the file picker: {e}");
            }
        })),
        write: Some(Box::new(|path: &str, bytes: &[u8]| save_to_downloads(&file_name(path), bytes))),
        download: Some(Box::new(|name: &str, bytes: &[u8]| {
            if let Err(e) = save_to_downloads(&file_name(name), bytes) {
                log::error!("saving {name} failed: {e}");
            }
        })),
        inbox: Some(inbox().clone()),
        ..Default::default()
    }
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string())
}

/// `ui.json` (UI state) and `prefs.json` (engine preferences) next to each other, as on the desktop.
fn load_prefs(app: &mut SlideApp, path: &Path) {
    if let Ok(bytes) = std::fs::read(path)
        && let Ok(ui) = serde_json::from_slice::<UiState>(&bytes)
    {
        app.ui = ui;
        // Recent entries are file names from the picker, not paths that can be reopened.
        app.ui.recent.clear();
    }
    if let Ok(bytes) = std::fs::read(path.with_file_name("prefs.json"))
        && let Ok(prefs) = serde_json::from_slice::<deckcraft_engine::Prefs>(&bytes)
    {
        app.session.prefs = prefs;
    }
}

/// Both preference files as one string, to notice changes cheaply.
fn prefs_text(app: &SlideApp) -> String {
    let ui = serde_json::to_string(&app.ui).unwrap_or_default();
    let prefs = serde_json::to_string(&app.session.prefs).unwrap_or_default();
    format!("{ui}\n{prefs}")
}

/// Write the UI state and preferences when they changed since the last write.
fn save_prefs(app: &SlideApp, path: &Path, last: &mut String) {
    let text = prefs_text(app);
    if text == *last {
        return;
    }
    let (ui, prefs) = text.split_once('\n').unwrap_or((&text, ""));
    match write_atomic(path, ui.as_bytes()).and_then(|()| write_atomic(&path.with_file_name("prefs.json"), prefs.as_bytes())) {
        Ok(()) => *last = text,
        Err(e) => log::warn!("couldn't save preferences: {e}"),
    }
}

/// Write `bytes` to `path` through a temporary file, so a crash mid-write keeps the old file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

// ---- Calls into MainActivity (Kotlin) ----------------------------------------------------------

/// Run `f` with a JNI environment on this thread and the current activity.
fn with_activity<T>(f: impl FnOnce(&mut Env<'_>, &JObject<'_>) -> jni::errors::Result<T>) -> Result<T, String> {
    let vm = VM.get().ok_or("the Java VM is not available")?;
    let raw = *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner);
    if raw == 0 {
        return Err("the activity is not running".to_string());
    }
    let raw = raw as jni::sys::jobject;
    vm.attach_current_thread(|env| -> jni::errors::Result<T> {
        // SAFETY: the reference comes from android-activity's `activity_as_ptr`, which keeps it
        // valid while the activity runs (ACTIVITY is cleared when `run_native` returns). `Cast`
        // neither owns nor deletes it.
        let activity = unsafe { env.as_cast_raw::<JObject>(&raw)? };
        f(env, &activity)
    })
    .map_err(|e| e.to_string())
}

/// `MainActivity.pickOpen()`: show the system file picker; the result comes through the inbox.
fn pick_open() -> Result<(), String> {
    with_activity(|env, activity| {
        env.call_method(activity, jni_str!("pickOpen"), jni_sig!("()V"), &[])?;
        Ok(())
    })
}

/// `MainActivity.saveToDownloads(name, bytes)`: `null` on success, else the error message.
fn save_to_downloads(name: &str, bytes: &[u8]) -> Result<(), String> {
    with_activity(|env, activity| {
        let jname = JString::from_str(env, name)?;
        let jbytes = env.byte_array_from_slice(bytes)?;
        let ret = env
            .call_method(activity, jni_str!("saveToDownloads"), jni_sig!("(Ljava/lang/String;[B)Ljava/lang/String;"), &[(&jname).into(), (&jbytes).into()])?
            .l()?;
        if ret.is_null() {
            return Ok(Ok(()));
        }
        let message = env.cast_local::<JString>(ret)?;
        Ok(Err(message.to_string()))
    })?
}

/// `MainActivity.openUrl(url)`: links (Help, Discord, hyperlinks) in the browser.
fn open_url(url: &str) -> Result<(), String> {
    with_activity(|env, activity| {
        let jurl = JString::from_str(env, url)?;
        env.call_method(activity, jni_str!("openUrl"), jni_sig!("(Ljava/lang/String;)V"), &[(&jurl).into()])?;
        Ok(())
    })
}

// ---- Calls from MainActivity (Kotlin) ----------------------------------------------------------

/// `MainActivity.nativeDeliverFile(name, bytes)`: a picked file's contents, from a Java thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_iameberhard_deckcraft_MainActivity_nativeDeliverFile<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _this: JObject<'caller>,
    name: JString<'caller>,
    bytes: JByteArray<'caller>,
) {
    let outcome = unowned_env.with_env(|env| -> jni::errors::Result<()> {
        let name = name.to_string();
        let bytes = env.convert_byte_array(&bytes)?;
        log::info!("received {name} ({} bytes)", bytes.len());
        inbox().lock().unwrap_or_else(PoisonError::into_inner).push((name, bytes));
        if let Some(ctx) = CTX.get() {
            ctx.request_repaint();
        }
        Ok(())
    });
    outcome.resolve::<jni::errors::LogErrorAndDefault>()
}
