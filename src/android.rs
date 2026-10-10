//! Android platform services: clipboard, toasts and the shared-content
//! outbox.
//!
//! The UI runs on the `android_main` thread; anything that must touch the
//! JVM (ClipboardManager, Toast) is bridged with JNI on the Java main thread
//! via `AndroidApp::run_on_java_main_thread`. Shared content is copied on a
//! worker thread so a large file never stalls either thread.
//!
//! android-activity runs `android_main` once per activity *instance*, all in
//! one process. The first instance owns the UI; the system share sheet,
//! however, starts a fresh instance inside the sharing app's task while ours
//! is running. Such a later instance only forwards its share to the UI and
//! closes itself (see `forward_to_primary`).
//!
//! On non-Android builds this module compiles to no-ops so the rest of the
//! app can reference it unconditionally.

#[cfg(target_os = "android")]
mod imp {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, OnceLock};
    use std::time::Duration;

    use jni::objects::{GlobalRef, JByteArray, JClass, JObject, JObjectArray, JString, JValue};
    use jni::{JNIEnv, JavaVM, NativeMethod};

    use slint::android::android_activity::{AndroidApp, MainEvent, PollEvent};

    static ANDROID_APP: OnceLock<AndroidApp> = OnceLock::new();

    /// Store the app handle for later platform calls. Called from
    /// `android_main` before anything else touches this module.
    pub fn set_app(app: AndroidApp) {
        let _ = ANDROID_APP.set(app);
    }

    fn app() -> &'static AndroidApp {
        ANDROID_APP.get().expect("AndroidApp not initialised")
    }

    // ----------------------------------------------------------- data dirs

    /// App-private storage (`internal_data_path` → /data/data/<pkg>/files).
    pub fn data_dir() -> PathBuf {
        let app = app();
        app.internal_data_path()
            .or_else(|| app.external_data_path())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// `Context.getCacheDir()`; app-private and writable on every version.
    fn cache_dir() -> Result<PathBuf, String> {
        with_env(|env| {
            let dir = env
                .call_method(&activity()?, "getCacheDir", "()Ljava/io/File;", &[])
                .map_err(|e| e.to_string())?
                .l()
                .map_err(|e| e.to_string())?;
            if dir.is_null() {
                return Err("no cache dir".to_string());
            }
            let path = env
                .call_method(&dir, "getAbsolutePath", "()Ljava/lang/String;", &[])
                .map_err(|e| e.to_string())?
                .l()
                .map_err(|e| e.to_string())?;
            Ok(PathBuf::from(
                env.get_string(&JString::from(path))
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .into_owned(),
            ))
        })
    }

    /// Point the engine's blob stores at the app cache dir. Before Android 13
    /// `std::env::temp_dir()` is `/data/local/tmp`, which apps cannot write:
    /// every send failed to set up and receives never started. Must run
    /// before the first transfer (and the history sweep, which scans it).
    pub fn use_cache_dir_for_blob_stores() {
        let dir = cache_dir().unwrap_or_else(|e| {
            tracing::warn!("cache dir unavailable ({e}); using the data dir");
            data_dir().join("cache")
        });
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(dir = %dir.display(), "cannot create blob store root: {e}");
        }
        tracing::info!(dir = %dir.display(), "blob stores live here");
        let _ = crate::engine::storage::TEMP_DIR.set(dir);
    }

    /// Send the app to the background, as Back does at an app's top level.
    /// Unlike letting Android finish the activity, the process (and any
    /// transfer in flight) keeps running.
    pub fn move_to_background() {
        let Some(app) = ANDROID_APP.get() else {
            return;
        };
        app.run_on_java_main_thread(Box::new(|| {
            let moved = with_env(|env| {
                env.call_method(&activity()?, "moveTaskToBack", "(Z)Z", &[JValue::Bool(1)])
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            });
            if let Err(e) = moved {
                tracing::warn!("moveTaskToBack failed: {e}");
            }
        }));
    }

    /// Default location for received files: the public Downloads folder, or
    /// the app-private one where Android won't let us write there (see
    /// `Storage.downloadsDir`).
    pub fn downloads_dir() -> PathBuf {
        let public = with_env(|env| {
            let class = storage_class(env)?;
            let dir = env
                .call_static_method(
                    class,
                    "downloadsDir",
                    "(Landroid/app/Activity;)Ljava/lang/String;",
                    &[JValue::Object(&activity()?)],
                )
                .map_err(|e| e.to_string())?
                .l()
                .map_err(|e| e.to_string())?;
            if dir.is_null() {
                return Ok(None);
            }
            let dir: String = env
                .get_string(&JString::from(dir))
                .map_err(|e| e.to_string())?
                .into();
            Ok(Some(PathBuf::from(dir)))
        });
        match public {
            Ok(Some(dir)) => return dir,
            Ok(None) => {}
            Err(e) => tracing::warn!("public downloads dir unavailable: {e}"),
        }
        data_dir().join("downloads")
    }

    /// Directory where shared-in content is staged before sending.
    pub fn outbox_dir() -> PathBuf {
        data_dir().join("outbox")
    }

    /// Files currently staged in the outbox, sorted.
    pub fn outbox_files() -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(outbox_dir())
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_file())
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        files
    }

    /// Remove staged outbox files after they have been shared.
    pub fn clear_outbox(paths: &[PathBuf]) {
        let dir = outbox_dir();
        for path in paths {
            if path.starts_with(&dir) {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    // ---------------------------------------------------------- JNI access

    /// Run `f` with a `JNIEnv` for the current thread.
    fn with_env<T>(f: impl FnOnce(&mut jni::JNIEnv) -> Result<T, String>) -> Result<T, String> {
        // SAFETY: `vm_as_ptr` returns the JavaVM pointer owned by the
        // Android runtime; it stays valid for the process lifetime.
        let vm = unsafe { JavaVM::from_raw(app().vm_as_ptr().cast()) }
            .map_err(|e| format!("JavaVM: {e}"))?;
        let mut guard = vm.attach_current_thread().map_err(|e| e.to_string())?;
        let env = &mut *guard;
        let result = f(env);
        // A failed call leaves its Java exception pending; any later JNI call
        // on this thread (or returning into Java) would then abort the app.
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_describe();
            let _ = env.exception_clear();
        }
        result
    }

    /// Activity object for JNI calls. `AndroidApp` holds a global reference
    /// that lives as long as the app handle; the returned lifetime is
    /// chosen by the caller's JNI scope.
    fn activity<'local>() -> Result<JObject<'local>, String> {
        activity_of(app())
    }

    /// Like [`activity`], for any activity instance (see `forward_to_primary`).
    fn activity_of<'local>(app: &AndroidApp) -> Result<JObject<'local>, String> {
        let raw = app.activity_as_ptr() as jni::sys::jobject;
        if raw.is_null() {
            return Err("activity handle is null".to_string());
        }
        // SAFETY: non-null global reference owned by AndroidApp (kept alive
        // in a process-wide OnceLock); we never wrap or delete it.
        Ok(unsafe { JObject::from_raw(raw) })
    }

    // ------------------------------------------------------ multicast lock

    /// Android drops inbound multicast without a `WifiManager.MulticastLock`.
    /// mDNS publish (outbound) works regardless, but receive needs the lock , 
    /// without it a peer sees this device while this device never sees them,
    /// and a pair request the peer sends is never noticed. The `GlobalRef`
    /// keeps the lock object (and thus the held lock) alive for the process
    /// lifetime.
    static MULTICAST_LOCK: OnceLock<GlobalRef> = OnceLock::new();

    /// Marks the launch intent as consumed; stages it exactly once per process.
    static INTENT_STAGED: OnceLock<()> = OnceLock::new();

    pub fn acquire_multicast_lock() {
        match with_env(acquire_multicast_lock_with_env) {
            Ok(lock) => {
                let _ = MULTICAST_LOCK.set(lock);
            }
            Err(e) => {
                tracing::warn!("multicast lock unavailable; mDNS receive may be unreliable: {e}")
            }
        }
    }

    fn acquire_multicast_lock_with_env(env: &mut jni::JNIEnv) -> Result<GlobalRef, String> {
        let activity = activity()?;
        // Context.WIFI_SERVICE == "wifi"
        let service_name = env.new_string("wifi").map_err(|e| e.to_string())?;
        let wifi = env
            .call_method(
                &activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[JValue::Object(&service_name)],
            )
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        if wifi.is_null() {
            return Err("WifiManager unavailable".to_string());
        }
        let tag = env
            .new_string("flipflop-mdns")
            .map_err(|e| e.to_string())?;
        let lock = env
            .call_method(
                &wifi,
                "createMulticastLock",
                "(Ljava/lang/String;)Landroid/net/wifi/WifiManager$MulticastLock;",
                &[JValue::Object(&tag)],
            )
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        // Reference-counted off: a single acquire holds until release, and the
        // lock is never released here, it lives for the whole process.
        env.call_method(&lock, "setReferenceCounted", "(Z)V", &[JValue::Bool(0)])
            .map_err(|e| e.to_string())?;
        env.call_method(&lock, "acquire", "()V", &[])
            .map_err(|e| e.to_string())?;
        env.new_global_ref(lock).map_err(|e| e.to_string())
    }

    // ----------------------------------------------------------- clipboard

    /// Read the clipboard's text; `done` runs on the Slint event loop with
    /// `None` when there is no text (or no access).
    pub fn read_clipboard(done: impl FnOnce(Option<String>) + Send + 'static) {
        let Some(app) = ANDROID_APP.get() else {
            done(None);
            return;
        };
        // The clipboard is only readable by the focused app, from its UI thread.
        app.run_on_java_main_thread(Box::new(move || {
            let text = with_env(clipboard_text).unwrap_or_else(|e| {
                tracing::warn!("clipboard read failed: {e}");
                None
            });
            let _ = slint::invoke_from_event_loop(move || done(text));
        }));
    }

    fn clipboard_text(env: &mut jni::JNIEnv) -> Result<Option<String>, String> {
        let activity = activity()?;
        // Context.CLIPBOARD_SERVICE == "clipboard"
        let name = env.new_string("clipboard").map_err(|e| e.to_string())?;
        let manager = env
            .call_method(
                &activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[JValue::Object(&name)],
            )
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        if manager.is_null() {
            return Ok(None);
        }
        let clip = env
            .call_method(
                &manager,
                "getPrimaryClip",
                "()Landroid/content/ClipData;",
                &[],
            )
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        if clip.is_null() {
            return Ok(None);
        }
        let count = env
            .call_method(&clip, "getItemCount", "()I", &[])
            .map_err(|e| e.to_string())?
            .i()
            .map_err(|e| e.to_string())?;
        if count == 0 {
            return Ok(None);
        }
        let item = env
            .call_method(
                &clip,
                "getItemAt",
                "(I)Landroid/content/ClipData$Item;",
                &[JValue::Int(0)],
            )
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        let text = env
            .call_method(
                &item,
                "coerceToText",
                "(Landroid/content/Context;)Ljava/lang/CharSequence;",
                &[JValue::Object(&activity)],
            )
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        if text.is_null() {
            return Ok(None);
        }
        let text = env
            .call_method(&text, "toString", "()Ljava/lang/String;", &[])
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        Ok(Some(
            env.get_string(&JString::from(text))
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .into_owned(),
        ))
    }

    /// Open a link in the user's browser (ACTION_VIEW).
    pub fn open_url(url: &str) {
        let Some(app) = ANDROID_APP.get() else {
            return;
        };
        let url = url.to_string();
        app.run_on_java_main_thread(Box::new(move || {
            let opened = with_env(|env| {
                let text = env.new_string(&url).map_err(|e| e.to_string())?;
                let uri = env
                    .call_static_method(
                        "android/net/Uri",
                        "parse",
                        "(Ljava/lang/String;)Landroid/net/Uri;",
                        &[JValue::Object(&text)],
                    )
                    .map_err(|e| e.to_string())?
                    .l()
                    .map_err(|e| e.to_string())?;
                let action = env
                    .new_string("android.intent.action.VIEW")
                    .map_err(|e| e.to_string())?;
                let intent = env
                    .new_object(
                        "android/content/Intent",
                        "(Ljava/lang/String;Landroid/net/Uri;)V",
                        &[JValue::Object(&action), JValue::Object(&uri)],
                    )
                    .map_err(|e| e.to_string())?;
                env.call_method(
                    &activity()?,
                    "startActivity",
                    "(Landroid/content/Intent;)V",
                    &[JValue::Object(&intent)],
                )
                .map(|_| ())
                .map_err(|e| e.to_string())
            });
            if let Err(e) = opened {
                tracing::warn!("opening {url} failed: {e}");
                show_toast("No app can open this link", true);
            }
        }));
    }

    pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
        let app = app().clone();
        let text = text.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        app.run_on_java_main_thread(Box::new(move || {
            let _ = tx.send(with_env(|env| clipboard_with_env(env, &text)));
        }));
        rx.recv()
            .map_err(|_| "clipboard task did not run".to_string())?
    }

    fn clipboard_with_env(env: &mut jni::JNIEnv, text: &str) -> Result<(), String> {
        let activity = activity()?;
        // Context.CLIPBOARD_SERVICE == "clipboard"
        let service = env.new_string("clipboard").map_err(|e| e.to_string())?;
        let cm = env
            .call_method(
                &activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[JValue::Object(&service)],
            )
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        let clip_class = env
            .find_class("android/content/ClipData")
            .map_err(|e| e.to_string())?;
        let label = env.new_string("flipflop").map_err(|e| e.to_string())?;
        let jtext = env.new_string(text).map_err(|e| e.to_string())?;
        // ClipData.newPlainText(label, text)
        let clip = env
            .call_static_method(
                clip_class,
                "newPlainText",
                "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Landroid/content/ClipData;",
                &[JValue::Object(&label), JValue::Object(&jtext)],
            )
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        env.call_method(
            &cm,
            "setPrimaryClip",
            "(Landroid/content/ClipData;)V",
            &[JValue::Object(&clip)],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    // -------------------------------------------------------------- toasts

    pub fn show_toast(msg: &str, _error: bool) {
        let Some(app) = ANDROID_APP.get() else {
            return;
        };
        let msg = msg.to_string();
        // Fire-and-forget: blocking here would deadlock the UI thread when
        // called from inside event-loop closures.
        app.run_on_java_main_thread(Box::new(move || {
            if let Err(e) = with_env(|env| toast_with_env(env, &msg)) {
                tracing::warn!("toast failed: {e}");
            }
        }));
    }

    fn toast_with_env(env: &mut jni::JNIEnv, msg: &str) -> Result<(), String> {
        let activity = activity()?;
        let toast_class = env
            .find_class("android/widget/Toast")
            .map_err(|e| e.to_string())?;
        let jmsg = env.new_string(msg).map_err(|e| e.to_string())?;
        // Toast.makeText(activity, text, Toast.LENGTH_LONG).show();
        let toast = env
            .call_static_method(
                toast_class,
                "makeText",
                "(Landroid/content/Context;Ljava/lang/CharSequence;I)Landroid/widget/Toast;",
                &[
                    JValue::Object(&activity),
                    JValue::Object(&jmsg),
                    JValue::Int(1),
                ],
            )
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        env.call_method(&toast, "show", "()V", &[])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    // ----------------------------------------------------- shared content

    thread_local! {
        /// Receives staged outbox files; lives on the UI (event loop) thread.
        static ON_SHARED: RefCell<Option<Box<dyn Fn(Vec<PathBuf>, Origin)>>> = RefCell::new(None);
    }

    /// How content got into the outbox.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum Origin {
        /// Shared from another app (the share sheet).
        Share,
        /// Picked with the in-app file picker.
        Picker,
    }

    /// Register the UI's handler for new outbox content: called on the event
    /// loop with every staged outbox file each time something is shared in or
    /// picked. Must be called on the UI thread.
    pub fn on_shared(handler: impl Fn(Vec<PathBuf>, Origin) + 'static) {
        ON_SHARED.with(|h| *h.borrow_mut() = Some(Box::new(handler)));
    }

    /// Hand the outbox to the UI's [`on_shared`] handler.
    fn deliver_outbox(origin: Origin) {
        let staged = outbox_files();
        let posted = slint::invoke_from_event_loop(move || {
            ON_SHARED.with(|h| {
                if let Some(handler) = h.borrow().as_ref() {
                    handler(staged, origin);
                }
            })
        });
        if let Err(e) = posted {
            tracing::warn!("could not report staged files: {e}");
        }
    }

    /// Shares-into-the-app entry point ("intent listener"): stages whatever the
    /// launch intent carries (ACTION_SEND / ACTION_SEND_MULTIPLE) into the
    /// outbox, then hands every staged file to the [`on_shared`] handler.
    ///
    /// A lazy SAF picker needs activity-result plumbing that android-activity
    /// does not forward, so "Send to flipflop" from the system share
    /// sheet is the supported flow on Android. Files are copied into the
    /// outbox and the share uses those ordinary paths.
    ///
    /// Never blocks: called at startup, before the event loop runs, while the
    /// Java main thread is still inside onStart/onResume waiting for this
    /// thread to acknowledge the lifecycle change. Waiting on it here would
    /// deadlock (black screen). A process-local guard keeps the same intent
    /// from being staged twice.
    pub fn stage_launch_intent() {
        if INTENT_STAGED.set(()).is_err() {
            return;
        }
        let app = app().clone();
        std::thread::spawn(move || {
            stage_intent_of(&app);
            deliver_outbox(Origin::Share);
        });
    }

    /// Copy what `app`'s launch intent shares into the outbox.
    fn stage_intent_of(app: &AndroidApp) {
        if let Err(e) = with_env(|env| collect_shared_files_with_env(env, &activity_of(app)?)) {
            tracing::warn!("staging shared files failed: {e}");
        }
    }

    /// `android_main` for every activity instance after the first.
    ///
    /// The share sheet starts a new instance in the sharing app's task while
    /// ours keeps running; launching the app while the first instance lives
    /// in another app's task (it was started by a share) does the same. This
    /// instance stages its intent's content for the running UI, brings the
    /// UI's task to the front and finishes itself.
    ///
    /// The instance must keep polling its events until destroyed: its Java
    /// lifecycle callbacks wait for this thread to acknowledge them.
    pub fn forward_to_primary(this: AndroidApp) {
        let worker = this.clone();
        std::thread::spawn(move || {
            // Stage before finishing: the sender's URI grant only lasts as
            // long as this activity does.
            stage_intent_of(&worker);
            deliver_outbox(Origin::Share);
            let closer = worker.clone();
            worker.run_on_java_main_thread(Box::new(move || {
                if let Err(e) = with_env(|env| show_primary_and_finish(env, &closer)) {
                    tracing::warn!("could not hand over to the main window: {e}");
                }
            }));
        });

        let mut destroyed = false;
        while !destroyed {
            this.poll_events(Some(Duration::from_millis(250)), |event| {
                if let PollEvent::Main(MainEvent::Destroy) = event {
                    destroyed = true;
                }
            });
        }
    }

    /// Move the UI's task to the front, then finish the forwarding activity
    /// (always, so a failed hand-over never leaves a dead window behind).
    fn show_primary_and_finish(env: &mut jni::JNIEnv, this: &AndroidApp) -> Result<(), String> {
        let forwarder = activity_of(this)?;
        let moved = move_primary_to_front(env, &forwarder);
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
        env.call_method(&forwarder, "finish", "()V", &[])
            .map_err(|e| e.to_string())?;
        moved
    }

    fn move_primary_to_front(env: &mut jni::JNIEnv, context: &JObject) -> Result<(), String> {
        let task_id = env
            .call_method(&activity()?, "getTaskId", "()I", &[])
            .map_err(|e| e.to_string())?
            .i()
            .map_err(|e| e.to_string())?;
        // Context.ACTIVITY_SERVICE == "activity"; needs REORDER_TASKS.
        let service_name = env.new_string("activity").map_err(|e| e.to_string())?;
        let manager = env
            .call_method(
                context,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[JValue::Object(&service_name)],
            )
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        if manager.is_null() {
            return Err("ActivityManager unavailable".to_string());
        }
        env.call_method(
            &manager,
            "moveTaskToFront",
            "(II)V",
            &[JValue::Int(task_id), JValue::Int(0)],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    // -------------------------------------------------------- file picker

    /// `android/java`, compiled by build.rs.
    const JAVA_HELPERS_DEX: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/classes.dex"));

    /// Class loader over the embedded dex, created once.
    static HELPERS_LOADER: OnceLock<GlobalRef> = OnceLock::new();

    fn helpers_loader(env: &mut JNIEnv) -> Result<&'static GlobalRef, String> {
        if let Some(loader) = HELPERS_LOADER.get() {
            return Ok(loader);
        }
        // SAFETY: the buffer is 'static and only ever read by the loader.
        let dex = unsafe {
            env.new_direct_byte_buffer(JAVA_HELPERS_DEX.as_ptr() as *mut u8, JAVA_HELPERS_DEX.len())
        }
        .map_err(|e| e.to_string())?;
        let parent = env
            .call_method(
                &activity()?,
                "getClassLoader",
                "()Ljava/lang/ClassLoader;",
                &[],
            )
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        let loader = env
            .new_object(
                "dalvik/system/InMemoryDexClassLoader",
                "(Ljava/nio/ByteBuffer;Ljava/lang/ClassLoader;)V",
                &[JValue::Object(&dex), JValue::Object(&parent)],
            )
            .map_err(|e| e.to_string())?;
        let loader = env.new_global_ref(loader).map_err(|e| e.to_string())?;
        Ok(HELPERS_LOADER.get_or_init(|| loader))
    }

    /// Loads helper class `name` from the embedded dex into `cell`, binding
    /// its native methods (a class from a runtime loader can't find JNI
    /// symbols by name).
    fn helper_class(
        env: &mut JNIEnv,
        cell: &'static OnceLock<GlobalRef>,
        name: &str,
        natives: &[NativeMethod],
    ) -> Result<&'static GlobalRef, String> {
        if let Some(class) = cell.get() {
            return Ok(class);
        }
        let loader = helpers_loader(env)?;
        let name = env.new_string(name).map_err(|e| e.to_string())?;
        let class: JClass = env
            .call_method(
                loader,
                "loadClass",
                "(Ljava/lang/String;)Ljava/lang/Class;",
                &[JValue::Object(&name)],
            )
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?
            .into();
        env.register_native_methods(&class, natives)
            .map_err(|e| e.to_string())?;
        let class = env.new_global_ref(class).map_err(|e| e.to_string())?;
        Ok(cell.get_or_init(|| class))
    }

    /// `com.flipflop.app.FilePicker`, loaded once from the embedded dex.
    static FILE_PICKER: OnceLock<GlobalRef> = OnceLock::new();

    fn file_picker_class(env: &mut JNIEnv) -> Result<&'static GlobalRef, String> {
        helper_class(
            env,
            &FILE_PICKER,
            "com.flipflop.app.FilePicker",
            &[NativeMethod {
                name: "onPicked".into(),
                sig: "([Ljava/lang/String;)V".into(),
                fn_ptr: on_picked as *mut std::ffi::c_void,
            }],
        )
    }

    // ------------------------------------------------------------ storage

    /// `com.flipflop.app.Storage`, loaded once from the embedded dex.
    static STORAGE: OnceLock<GlobalRef> = OnceLock::new();

    fn storage_class(env: &mut JNIEnv) -> Result<&'static GlobalRef, String> {
        helper_class(env, &STORAGE, "com.flipflop.app.Storage", &[])
    }

    /// Open a received file in the app the user picks.
    pub fn open_file(path: &Path) {
        let path = path.to_string_lossy().into_owned();
        let opened = with_env(|env| {
            let class = storage_class(env)?;
            let path = env.new_string(path).map_err(|e| e.to_string())?;
            env.call_static_method(
                class,
                "open",
                "(Landroid/app/Activity;Ljava/lang/String;)V",
                &[JValue::Object(&activity()?), JValue::Object(&path)],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        });
        if let Err(e) = opened {
            tracing::warn!("open file failed: {e}");
            show_toast("Could not open the file", true);
        }
    }

    /// Offer files to other apps through the share sheet.
    pub fn share_files(paths: &[PathBuf]) {
        let shared = with_env(|env| {
            let class = storage_class(env)?;
            let array = env
                .new_object_array(paths.len() as i32, "java/lang/String", JObject::null())
                .map_err(|e| e.to_string())?;
            for (i, path) in paths.iter().enumerate() {
                let path = env
                    .new_string(path.to_string_lossy())
                    .map_err(|e| e.to_string())?;
                env.set_object_array_element(&array, i as i32, path)
                    .map_err(|e| e.to_string())?;
            }
            env.call_static_method(
                class,
                "share",
                "(Landroid/app/Activity;[Ljava/lang/String;)V",
                &[JValue::Object(&activity()?), JValue::Object(&array)],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        });
        if let Err(e) = shared {
            tracing::warn!("share files failed: {e}");
            show_toast("Could not share the files", true);
        }
    }

    /// Offer plain text to other apps through the share sheet.
    pub fn share_text(text: &str) {
        let shared = with_env(|env| {
            let class = storage_class(env)?;
            let text = env.new_string(text).map_err(|e| e.to_string())?;
            env.call_static_method(
                class,
                "shareText",
                "(Landroid/app/Activity;Ljava/lang/String;)V",
                &[JValue::Object(&activity()?), JValue::Object(&text)],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        });
        if let Err(e) = shared {
            tracing::warn!("share text failed: {e}");
            show_toast("Could not share the text", true);
        }
    }

    /// Open the system file picker; picked files are staged into the outbox
    /// and reported to the [`on_shared`] handler (as [`Origin::Picker`]).
    pub fn pick_files() {
        let Some(app) = ANDROID_APP.get() else {
            return;
        };
        app.run_on_java_main_thread(Box::new(|| {
            let opened = with_env(|env| {
                let class = file_picker_class(env)?;
                env.call_static_method(
                    class,
                    "pick",
                    "(Landroid/app/Activity;)V",
                    &[JValue::Object(&activity()?)],
                )
                .map(|_| ())
                .map_err(|e| e.to_string())
            });
            if let Err(e) = opened {
                tracing::warn!("file picker failed: {e}");
                show_toast("Could not open the file picker", true);
            }
        }));
    }

    /// `FilePicker.onPicked`, on the Java main thread. Copies the picked
    /// files on a worker thread: their URI grants last while the activity
    /// lives, and big files would stall the UI.
    extern "system" fn on_picked<'local>(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        uris: JObjectArray<'local>,
    ) {
        let mut picked = Vec::new();
        let len = env.get_array_length(&uris).unwrap_or(0);
        for i in 0..len {
            let Ok(item) = env.get_object_array_element(&uris, i) else {
                continue;
            };
            if let Ok(uri) = env.get_string(&JString::from(item)) {
                picked.push(String::from(uri));
            }
        }
        if picked.is_empty() {
            return;
        }
        std::thread::spawn(move || {
            let staged = with_env(|env| {
                let mut parsed = Vec::new();
                for uri in &picked {
                    let text = env.new_string(uri).map_err(|e| e.to_string())?;
                    let uri = env
                        .call_static_method(
                            "android/net/Uri",
                            "parse",
                            "(Ljava/lang/String;)Landroid/net/Uri;",
                            &[JValue::Object(&text)],
                        )
                        .map_err(|e| e.to_string())?
                        .l()
                        .map_err(|e| e.to_string())?;
                    parsed.push(uri);
                }
                stage_uris(env, &activity()?, parsed)
            });
            if let Err(e) = staged {
                tracing::warn!("staging picked files failed: {e}");
                show_toast("Could not read the picked files", true);
            }
            deliver_outbox(Origin::Picker);
        });
    }

    // ----------------------------------------------------------- bluetooth

    /// `com.flipflop.app.BluetoothLink`, loaded once from the embedded dex.
    static BLUETOOTH_LINK: OnceLock<GlobalRef> = OnceLock::new();

    fn bluetooth_link_class(env: &mut JNIEnv) -> Result<&'static GlobalRef, String> {
        helper_class(
            env,
            &BLUETOOTH_LINK,
            "com.flipflop.app.BluetoothLink",
            &[
                NativeMethod {
                    name: "onPacket".into(),
                    sig: "([B[B)V".into(),
                    fn_ptr: bt_on_packet as *mut std::ffi::c_void,
                },
                NativeMethod {
                    name: "onPeerSeen".into(),
                    sig: "([B)V".into(),
                    fn_ptr: bt_on_peer_seen as *mut std::ffi::c_void,
                },
                NativeMethod {
                    name: "onAvailable".into(),
                    sig: "(Z)V".into(),
                    fn_ptr: bt_on_available as *mut std::ffi::c_void,
                },
            ],
        )
    }

    /// The process's Bluetooth transport, or `None` when this device can't
    /// carry it (before Android 10, or no Bluetooth LE). One per process:
    /// the link behind it starts once.
    pub fn bluetooth_hub() -> Option<crate::engine::BluetoothHub> {
        static HUB: OnceLock<Option<crate::engine::BluetoothHub>> = OnceLock::new();
        HUB.get_or_init(|| {
            let supported = with_env(|env| {
                let class = bluetooth_link_class(env)?;
                env.call_static_method(
                    class,
                    "supported",
                    "(Landroid/app/Activity;)Z",
                    &[JValue::Object(&activity()?)],
                )
                .and_then(|v| v.z())
                .map_err(|e| e.to_string())
            });
            match supported {
                Ok(true) => Some(crate::engine::BluetoothHub::new(Arc::new(AndroidBluetooth::new()))),
                Ok(false) => {
                    tracing::info!("no Bluetooth LE L2CAP on this device; transport off");
                    None
                }
                Err(e) => {
                    tracing::warn!("Bluetooth check failed; transport off: {e}");
                    None
                }
            }
        })
        .clone()
    }

    /// Where `BluetoothLink`'s callbacks deliver; set when the link starts.
    static BT_HUB: OnceLock<crate::engine::BluetoothHub> = OnceLock::new();

    /// Packets waiting for the `bt-send` thread. Bounded: a full queue drops,
    /// as a busy UDP socket would.
    const BT_SEND_QUEUE: usize = 256;

    /// [`crate::engine::BluetoothLink`] over `BluetoothLink.java`. Sends go through
    /// one thread permanently attached to the JVM, keeping JNI off iroh's
    /// send path.
    #[derive(Debug)]
    struct AndroidBluetooth {
        outgoing: std::sync::mpsc::SyncSender<(crate::engine::PeerTag, Vec<u8>)>,
    }

    impl AndroidBluetooth {
        fn new() -> Self {
            let (outgoing, packets) = std::sync::mpsc::sync_channel(BT_SEND_QUEUE);
            std::thread::Builder::new()
                .name("bt-send".into())
                .spawn(move || bt_send_loop(packets))
                .expect("spawn bt-send");
            Self { outgoing }
        }
    }

    impl crate::engine::BluetoothLink for AndroidBluetooth {
        fn start(&self, local: crate::engine::PeerTag, hub: crate::engine::BluetoothHub) {
            let _ = BT_HUB.set(hub);
            app().run_on_java_main_thread(Box::new(move || {
                let started = with_env(|env| {
                    let class = bluetooth_link_class(env)?;
                    let tag = env.byte_array_from_slice(&local).map_err(|e| e.to_string())?;
                    env.call_static_method(
                        class,
                        "start",
                        "(Landroid/app/Activity;[B)V",
                        &[JValue::Object(&activity()?), JValue::Object(&tag)],
                    )
                    .map(|_| ())
                    .map_err(|e| e.to_string())
                });
                if let Err(e) = started {
                    tracing::warn!("Bluetooth link failed to start: {e}");
                }
            }));
        }

        fn send(&self, peer: crate::engine::PeerTag, packet: &[u8]) {
            let _ = self.outgoing.try_send((peer, packet.to_vec()));
        }
    }

    fn bt_send_loop(packets: std::sync::mpsc::Receiver<(crate::engine::PeerTag, Vec<u8>)>) {
        // SAFETY: as in `with_env`.
        let vm = match unsafe { JavaVM::from_raw(app().vm_as_ptr().cast()) } {
            Ok(vm) => vm,
            Err(e) => return tracing::warn!("bt-send: no JavaVM: {e}"),
        };
        let mut env = match vm.attach_current_thread_permanently() {
            Ok(env) => env,
            Err(e) => return tracing::warn!("bt-send: cannot attach: {e}"),
        };
        let class = match bluetooth_link_class(&mut env) {
            Ok(class) => class,
            Err(e) => return tracing::warn!("bt-send: no BluetoothLink: {e}"),
        };
        for (tag, packet) in packets {
            // A local frame per packet: this thread never returns to Java,
            // so local references would otherwise pile up.
            let sent = env.with_local_frame(4, |env| -> jni::errors::Result<()> {
                let tag = env.byte_array_from_slice(&tag)?;
                let packet = env.byte_array_from_slice(&packet)?;
                env.call_static_method(
                    class,
                    "send",
                    "([B[B)V",
                    &[JValue::Object(&tag), JValue::Object(&packet)],
                )?;
                Ok(())
            });
            if sent.is_err() && env.exception_check().unwrap_or(false) {
                let _ = env.exception_describe();
                let _ = env.exception_clear();
            }
        }
    }

    fn peer_tag_from(env: &mut JNIEnv, array: &JByteArray) -> Option<crate::engine::PeerTag> {
        env.convert_byte_array(array).ok()?.try_into().ok()
    }

    /// `BluetoothLink.onPacket`, on a link's reader thread.
    extern "system" fn bt_on_packet<'local>(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        from: JByteArray<'local>,
        packet: JByteArray<'local>,
    ) {
        let (Some(hub), Some(from)) = (BT_HUB.get(), peer_tag_from(&mut env, &from)) else {
            return;
        };
        if let Ok(packet) = env.convert_byte_array(&packet) {
            hub.deliver(from, packet);
        }
    }

    /// `BluetoothLink.onPeerSeen`, on the scan callback thread.
    extern "system" fn bt_on_peer_seen<'local>(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        tag: JByteArray<'local>,
    ) {
        if let (Some(hub), Some(tag)) = (BT_HUB.get(), peer_tag_from(&mut env, &tag)) {
            hub.peer_seen(tag);
        }
    }

    /// `BluetoothLink.onAvailable`, whenever the link comes up or goes down.
    extern "system" fn bt_on_available<'local>(
        _env: JNIEnv<'local>,
        _class: JClass<'local>,
        available: jni::sys::jboolean,
    ) {
        let available = available != 0;
        tracing::info!(available, "Bluetooth transport");
        if let Some(hub) = BT_HUB.get() {
            hub.set_available(available);
        }
    }

    // ------------------------------------------------------------- network

    /// `com.flipflop.app.NetworkWatch`, loaded once from the embedded dex.
    static NETWORK_WATCH: OnceLock<GlobalRef> = OnceLock::new();

    /// Where `NetworkWatch.onNetworkChanged` delivers; set by `watch_network`.
    static ON_NETWORK_CHANGED: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

    /// Calls `on_change` (on a binder thread) whenever a network comes, goes
    /// or changes address, in bursts. iroh can't see these on Android, so
    /// the node must be told. Once per process; later calls are ignored.
    pub fn watch_network(on_change: impl Fn() + Send + Sync + 'static) {
        if ON_NETWORK_CHANGED.set(Box::new(on_change)).is_err() {
            return;
        }
        let started = with_env(|env| {
            let class = helper_class(
                env,
                &NETWORK_WATCH,
                "com.flipflop.app.NetworkWatch",
                &[NativeMethod {
                    name: "onNetworkChanged".into(),
                    sig: "()V".into(),
                    fn_ptr: net_on_changed as *mut std::ffi::c_void,
                }],
            )?;
            env.call_static_method(
                class,
                "start",
                "(Landroid/content/Context;)V",
                &[JValue::Object(&activity()?)],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        });
        if let Err(e) = started {
            tracing::warn!("cannot watch the network; changes go unnoticed: {e}");
        }
    }

    /// `NetworkWatch.onNetworkChanged`, on a ConnectivityManager binder thread.
    extern "system" fn net_on_changed<'local>(_env: JNIEnv<'local>, _class: JClass<'local>) {
        if let Some(on_change) = ON_NETWORK_CHANGED.get() {
            on_change();
        }
    }

    fn collect_shared_files_with_env(
        env: &mut jni::JNIEnv,
        activity: &JObject,
    ) -> Result<Vec<PathBuf>, String> {
        let intent = env
            .call_method(activity, "getIntent", "()Landroid/content/Intent;", &[])
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        if intent.is_null() {
            return Ok(Vec::new());
        }
        let action_obj = env
            .call_method(&intent, "getAction", "()Ljava/lang/String;", &[])
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        let action = if action_obj.is_null() {
            String::new()
        } else {
            env.get_string(&JString::from(action_obj))
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .into_owned()
        };

        let key = env
            .new_string("android.intent.extra.STREAM")
            .map_err(|e| e.to_string())?;
        let mut uris: Vec<JObject> = Vec::new();
        match action.as_str() {
            // ACTION_SEND: a single Uri stream.
            "android.intent.action.SEND" => {
                let extra = env
                    .call_method(
                        &intent,
                        "getParcelableExtra",
                        "(Ljava/lang/String;)Landroid/os/Parcelable;",
                        &[JValue::Object(&key)],
                    )
                    .map_err(|e| e.to_string())?
                    .l()
                    .map_err(|e| e.to_string())?;
                if !extra.is_null() {
                    uris.push(extra);
                }
            }
            // ACTION_SEND_MULTIPLE: an ArrayList<Uri>.
            "android.intent.action.SEND_MULTIPLE" => {
                let list = env
                    .call_method(
                        &intent,
                        "getSerializableExtra",
                        "(Ljava/lang/String;)Ljava/io/Serializable;",
                        &[JValue::Object(&key)],
                    )
                    .map_err(|e| e.to_string())?
                    .l()
                    .map_err(|e| e.to_string())?;
                if !list.is_null() {
                    let size = env
                        .call_method(&list, "size", "()I", &[])
                        .map_err(|e| e.to_string())?
                        .i()
                        .map_err(|e| e.to_string())?;
                    for i in 0..size {
                        let item = env
                            .call_method(&list, "get", "(I)Ljava/lang/Object;", &[JValue::Int(i)])
                            .map_err(|e| e.to_string())?
                            .l()
                            .map_err(|e| e.to_string())?;
                        if !item.is_null() {
                            uris.push(item);
                        }
                    }
                }
            }
            _ => {}
        }
        if uris.is_empty() {
            // Shared text or a link (ACTION_SEND with EXTRA_TEXT only).
            if action == "android.intent.action.SEND" {
                return stage_text(env, &intent).map(|p| p.into_iter().collect());
            }
            return Ok(Vec::new());
        }
        stage_uris(env, activity, uris)
    }

    /// Save a text share (`EXTRA_TEXT`, named after `EXTRA_SUBJECT` when
    /// given) as a `.txt` file in the outbox.
    fn stage_text(env: &mut jni::JNIEnv, intent: &JObject) -> Result<Option<PathBuf>, String> {
        let Some(text) = string_extra(env, intent, "android.intent.extra.TEXT")? else {
            return Ok(None);
        };
        if text.trim().is_empty() {
            return Ok(None);
        }
        let subject = string_extra(env, intent, "android.intent.extra.SUBJECT")?
            .map(|s| s.trim().chars().take(60).collect::<String>())
            .filter(|s| !s.is_empty());
        let name = match subject {
            Some(subject) => format!("{subject}.txt"),
            None if text.trim().starts_with("http") => "shared-link.txt".to_string(),
            None => "shared-text.txt".to_string(),
        };
        let path = unique_outbox_path(&name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, text.trim()).map_err(|e| e.to_string())?;
        Ok(Some(path))
    }

    /// `intent.getCharSequenceExtra(key)` as a string.
    fn string_extra(
        env: &mut jni::JNIEnv,
        intent: &JObject,
        key: &str,
    ) -> Result<Option<String>, String> {
        let jkey = env.new_string(key).map_err(|e| e.to_string())?;
        let value = env
            .call_method(
                intent,
                "getCharSequenceExtra",
                "(Ljava/lang/String;)Ljava/lang/CharSequence;",
                &[JValue::Object(&jkey)],
            )
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        if value.is_null() {
            return Ok(None);
        }
        let text = env
            .call_method(&value, "toString", "()Ljava/lang/String;", &[])
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        Ok(Some(
            env.get_string(&JString::from(text))
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .into_owned(),
        ))
    }

    fn stage_uris(
        env: &mut jni::JNIEnv,
        activity: &JObject,
        uris: Vec<JObject>,
    ) -> Result<Vec<PathBuf>, String> {
        let resolver = env
            .call_method(
                activity,
                "getContentResolver",
                "()Landroid/content/ContentResolver;",
                &[],
            )
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        let mut staged = Vec::new();
        for uri in &uris {
            let name = uri_display_name(env, &resolver, uri)?;
            let input = env
                .call_method(
                    &resolver,
                    "openInputStream",
                    "(Landroid/net/Uri;)Ljava/io/InputStream;",
                    &[JValue::Object(uri)],
                )
                .map_err(|e| e.to_string())?
                .l()
                .map_err(|e| e.to_string())?;
            if input.is_null() {
                continue;
            }
            let out_path = unique_outbox_path(&name);
            let result = copy_stream_to_file(env, &input, &out_path);
            let _ = env.call_method(&input, "close", "()V", &[]);
            result?;
            staged.push(out_path);
        }
        Ok(staged)
    }

    /// The file name the sharing app advertises (`OpenableColumns.DISPLAY_NAME`);
    /// falls back to the URI's last path segment, which for `content://` URIs
    /// is usually an opaque id like `image:1234`.
    fn uri_display_name(
        env: &mut jni::JNIEnv,
        resolver: &JObject,
        uri: &JObject,
    ) -> Result<String, String> {
        match query_display_name(env, resolver, uri) {
            Ok(Some(name)) if !name.trim().is_empty() => return Ok(name),
            Ok(_) => {}
            Err(e) => {
                let _ = env.exception_clear();
                tracing::debug!("display name query failed: {e}");
            }
        }
        let seg = env
            .call_method(uri, "getLastPathSegment", "()Ljava/lang/String;", &[])
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        if seg.is_null() {
            return Ok("shared-file".to_string());
        }
        Ok(env
            .get_string(&JString::from(seg))
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned())
    }

    fn query_display_name(
        env: &mut jni::JNIEnv,
        resolver: &JObject,
        uri: &JObject,
    ) -> Result<Option<String>, String> {
        let column = env.new_string("_display_name").map_err(|e| e.to_string())?;
        let projection = env
            .new_object_array(1, "java/lang/String", &column)
            .map_err(|e| e.to_string())?;
        let null = JObject::null();
        // resolver.query(uri, {"_display_name"}, null, null, null)
        let cursor = env
            .call_method(
                resolver,
                "query",
                "(Landroid/net/Uri;[Ljava/lang/String;Ljava/lang/String;[Ljava/lang/String;Ljava/lang/String;)Landroid/database/Cursor;",
                &[
                    JValue::Object(uri),
                    JValue::Object(&projection),
                    JValue::Object(&null),
                    JValue::Object(&null),
                    JValue::Object(&null),
                ],
            )
            .map_err(|e| e.to_string())?
            .l()
            .map_err(|e| e.to_string())?;
        if cursor.is_null() {
            return Ok(None);
        }
        let name = (|| {
            let has_row = env
                .call_method(&cursor, "moveToFirst", "()Z", &[])
                .map_err(|e| e.to_string())?
                .z()
                .map_err(|e| e.to_string())?;
            if !has_row {
                return Ok(None);
            }
            let value = env
                .call_method(
                    &cursor,
                    "getString",
                    "(I)Ljava/lang/String;",
                    &[JValue::Int(0)],
                )
                .map_err(|e| e.to_string())?
                .l()
                .map_err(|e| e.to_string())?;
            if value.is_null() {
                return Ok(None);
            }
            env.get_string(&JString::from(value))
                .map(|s| Some(s.to_string_lossy().into_owned()))
                .map_err(|e| e.to_string())
        })();
        let _ = env.exception_clear();
        let _ = env.call_method(&cursor, "close", "()V", &[]);
        name
    }

    fn unique_outbox_path(file_name: &str) -> PathBuf {
        let dir = outbox_dir();
        let safe: String = file_name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let mut dest = dir.join(&safe);
        let mut n = 1;
        while dest.exists() {
            let stem = Path::new(&safe)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| safe.clone());
            let ext = Path::new(&safe)
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default();
            dest = dir.join(if ext.is_empty() {
                format!("{stem}-{n}")
            } else {
                format!("{stem}-{n}.{ext}")
            });
            n += 1;
        }
        dest
    }

    fn copy_stream_to_file(
        env: &mut jni::JNIEnv,
        input: &JObject,
        dest: &Path,
    ) -> Result<(), String> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        use std::io::Write;
        let file = std::fs::File::create(dest).map_err(|e| e.to_string())?;
        let mut writer = std::io::BufWriter::new(file);
        let buf = env.new_byte_array(64 * 1024).map_err(|e| e.to_string())?;
        let buf_obj = buf.as_ref();
        let mut chunk = vec![0i8; 64 * 1024];
        loop {
            let n = env
                .call_method(input, "read", "([B)I", &[JValue::Object(buf_obj)])
                .map_err(|e| e.to_string())?
                .i()
                .map_err(|e| e.to_string())?;
            if n <= 0 {
                break;
            }
            let chunk = &mut chunk[..n as usize];
            env.get_byte_array_region(&buf, 0, chunk)
                .map_err(|e| e.to_string())?;
            // jbyte is i8; reinterpret as raw bytes for the file.
            let bytes: Vec<u8> = chunk.iter().map(|b| *b as u8).collect();
            writer.write_all(&bytes).map_err(|e| e.to_string())?;
        }
        writer.flush().map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(target_os = "android")]
pub use imp::*;

#[cfg(not(target_os = "android"))]
mod imp {
    use std::path::PathBuf;

    /// No-op on desktop (the Android app handle is never set there).
    pub fn show_toast(_msg: &str, _error: bool) {}

    /// Always empty on desktop (no share-sheet staging).
    pub fn outbox_files() -> Vec<PathBuf> {
        Vec::new()
    }

    /// Unused on desktop.
    pub fn clear_outbox(_paths: &[PathBuf]) {}

    /// Unused on desktop (Back is an Android key).
    pub fn move_to_background() {}

    /// No Bluetooth transport on desktop yet.
    pub fn bluetooth_hub() -> Option<crate::engine::BluetoothHub> {
        None
    }

    /// Unused on desktop: iroh watches the network itself there.
    pub fn watch_network(_on_change: impl Fn() + Send + Sync + 'static) {}
}
#[cfg(not(target_os = "android"))]
pub use imp::*;

// ---------------------------------------------------------------- logcat

/// `tracing` writer that sends each formatted event to logcat (tag
/// `flipflop`) with the event's level as the log priority.
#[cfg(target_os = "android")]
pub struct Logcat;

#[cfg(target_os = "android")]
pub struct LogcatLine {
    priority: std::ffi::c_int,
    buf: Vec<u8>,
}

#[cfg(target_os = "android")]
#[link(name = "log")]
extern "C" {
    fn __android_log_write(
        prio: std::ffi::c_int,
        tag: *const std::ffi::c_char,
        text: *const std::ffi::c_char,
    ) -> std::ffi::c_int;
}

#[cfg(target_os = "android")]
impl std::io::Write for LogcatLine {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(target_os = "android")]
impl Drop for LogcatLine {
    // The fmt layer writes one whole event, then drops the writer.
    fn drop(&mut self) {
        let text = String::from_utf8_lossy(&self.buf).replace('\0', "");
        let text = text.trim_end();
        if text.is_empty() {
            return;
        }
        let Ok(text) = std::ffi::CString::new(text) else {
            return;
        };
        // SAFETY: both pointers are valid NUL-terminated strings for the call.
        unsafe {
            __android_log_write(self.priority, c"flipflop".as_ptr(), text.as_ptr());
        }
    }
}

#[cfg(target_os = "android")]
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logcat {
    type Writer = LogcatLine;

    fn make_writer(&'a self) -> LogcatLine {
        LogcatLine {
            priority: 4, // ANDROID_LOG_INFO
            buf: Vec::new(),
        }
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> LogcatLine {
        let priority = match *meta.level() {
            tracing::Level::ERROR => 6,
            tracing::Level::WARN => 5,
            tracing::Level::INFO => 4,
            tracing::Level::DEBUG => 3,
            tracing::Level::TRACE => 2,
        };
        LogcatLine {
            priority,
            buf: Vec::new(),
        }
    }
}

// ------------------------------------------------------------- entry point

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: slint::android::android_activity::AndroidApp) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static PRIMARY_STARTED: AtomicBool = AtomicBool::new(false);

    // A later activity instance (share sheet while running): Slint, logging
    // and the node already belong to the first one.
    if PRIMARY_STARTED.swap(true, Ordering::SeqCst) {
        imp::forward_to_primary(app);
        return;
    }
    slint::android::init(app.clone()).expect("Slint Android init");
    imp::set_app(app);
    imp::acquire_multicast_lock();
    crate::app::run();
    // Slint's loop ends when the activity is destroyed. Exit rather than
    // linger without a UI: the next launch then starts a fresh primary
    // instead of being mistaken for a share to forward.
    std::process::exit(0);
}
