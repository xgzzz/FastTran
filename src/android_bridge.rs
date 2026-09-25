#![allow(improper_ctypes_definitions)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use android_activity::AndroidApp;
use jni::objects::{JObject, JString};
use jni::refs::Global;
use jni::signature::RuntimeMethodSignature;
use jni::strings::JNIString;
use jni::{JValue, JavaVM};

#[derive(Debug)]
pub struct AndroidNotification {
    pub message: String,
    pub is_error: bool,
}

static APP: OnceLock<Mutex<Option<AndroidApp>>> = OnceLock::new();
static PICKED_FILES: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
static NOTIFICATIONS: OnceLock<Mutex<Vec<AndroidNotification>>> = OnceLock::new();
static SAFE_INSETS: OnceLock<Mutex<[f32; 4]>> = OnceLock::new();

fn app_slot() -> &'static Mutex<Option<AndroidApp>> {
    APP.get_or_init(|| Mutex::new(None))
}

fn picked_files_slot() -> &'static Mutex<Vec<PathBuf>> {
    PICKED_FILES.get_or_init(|| Mutex::new(Vec::new()))
}

fn notifications_slot() -> &'static Mutex<Vec<AndroidNotification>> {
    NOTIFICATIONS.get_or_init(|| Mutex::new(Vec::new()))
}

fn safe_insets_slot() -> &'static Mutex<[f32; 4]> {
    SAFE_INSETS.get_or_init(|| Mutex::new([24.0, 0.0, 0.0, 0.0]))
}

pub fn set_app(app: AndroidApp) {
    *app_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(app);
}

pub fn safe_insets() -> [f32; 4] {
    *safe_insets_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn take_picked_files() -> Vec<PathBuf> {
    let mut files = picked_files_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::mem::take(&mut *files)
}

pub fn take_notifications() -> Vec<AndroidNotification> {
    let mut notifications = notifications_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::mem::take(&mut *notifications)
}

fn push_notification(message: impl Into<String>, is_error: bool) {
    notifications_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(AndroidNotification {
            message: message.into(),
            is_error,
        });
}

fn with_activity<F>(app: &AndroidApp, callback: F) -> Result<(), String>
where
    F: for<'local> FnOnce(&mut jni::Env<'local>, &JObject<'local>) -> jni::errors::Result<()>,
{
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let raw_activity = app.activity_as_ptr() as jni::sys::jobject;
    vm.attach_current_thread(|env| {
        // AndroidApp exposes an unowned global reference. Cast it without
        // taking ownership, then make a short-lived owned global for the
        // duration of the Java method call.
        let activity_ref = unsafe { env.as_cast_raw::<Global<JObject>>(&raw_activity)? };
        let activity = env.new_global_ref(activity_ref.as_ref())?;
        callback(env, activity.as_ref())
    })
    .map_err(|error| error.to_string())
}

enum JavaArgument {
    String(String),
    Bool(bool),
}

fn call_void_method(
    app: &AndroidApp,
    method_name: &str,
    signature: &str,
    argument: Option<JavaArgument>,
) -> Result<(), String> {
    with_activity(app, |env, activity| {
        let method_name = JNIString::new(method_name);
        let runtime_signature = RuntimeMethodSignature::from_str(signature)?;
        let method_signature = runtime_signature.method_signature();
        match argument {
            Some(JavaArgument::String(value)) => {
                let value = env.new_string(value)?;
                env.call_method(
                    activity,
                    &method_name,
                    method_signature,
                    &[JValue::Object(value.as_ref())],
                )?;
            }
            Some(JavaArgument::Bool(value)) => {
                env.call_method(
                    activity,
                    &method_name,
                    method_signature,
                    &[JValue::Bool(value)],
                )?;
            }
            None => {
                env.call_method(activity, &method_name, method_signature, &[])?;
            }
        }
        Ok(())
    })
}

fn schedule_call<F>(
    method: &'static str,
    signature: &'static str,
    argument: Option<JavaArgument>,
    callback: F,
) where
    F: FnOnce(Result<(), String>) + Send + 'static,
{
    let app = match app_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        Some(app) => app,
        None => {
            callback(Err("Android Activity 尚未初始化".to_owned()));
            return;
        }
    };
    let callback_app = app.clone();
    app.run_on_java_main_thread(Box::new(move || {
        let result = call_void_method(&callback_app, method, signature, argument);
        callback(result);
    }));
}

pub fn choose_files() -> Result<(), String> {
    let app = app_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
        .ok_or_else(|| "Android Activity 尚未初始化".to_owned())?;
    let callback_app = app.clone();
    app.run_on_java_main_thread(Box::new(move || {
        if let Err(error) = call_void_method(&callback_app, "chooseFiles", "()V", None) {
            push_notification(error, true);
        }
    }));
    Ok(())
}

pub fn open_path(path: &Path) -> Result<(), String> {
    let path = path.to_string_lossy().into_owned();
    schedule_call(
        "openPath",
        "(Ljava/lang/String;)V",
        Some(JavaArgument::String(path)),
        |result| {
            if let Err(error) = result {
                push_notification(format!("无法打开文件：{error}"), true);
            }
        },
    );
    Ok(())
}

pub fn remove_picked_file(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(%error, "failed to remove cached picked file");
    }
    let path = path.to_string_lossy().into_owned();
    schedule_call(
        "releasePickedFile",
        "(Ljava/lang/String;)V",
        Some(JavaArgument::String(path)),
        |result| {
            if let Err(error) = result {
                tracing::debug!(%error, "failed to release Android document descriptor");
            }
        },
    );
}

pub fn set_system_bar_style(dark_mode: bool) {
    schedule_call(
        "applySystemBarStyle",
        "(Z)V",
        Some(JavaArgument::Bool(dark_mode)),
        |result| {
            if let Err(error) = result {
                tracing::debug!(%error, "failed to update Android system bar style");
            }
        },
    );
}

pub fn set_safe_insets(top: f32, right: f32, bottom: f32, left: f32) {
    *safe_insets_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = [top, right, bottom, left];
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_fasttran_app_FastTranActivity_nativeOnPickedFile<'caller>(
    mut env: jni::EnvUnowned<'caller>,
    _class: jni::objects::JClass<'caller>,
    path: JString<'caller>,
) {
    let outcome = env.with_env(|env| -> jni::errors::Result<()> {
        let Ok(path) = path.try_to_string(env) else {
            push_notification("文件选择器返回了无效路径", true);
            return Ok(());
        };
        picked_files_slot()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(PathBuf::from(path));
        Ok(())
    });
    outcome.resolve::<jni::errors::ThrowRuntimeExAndDefault>();
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_fasttran_app_FastTranActivity_nativeOnPickerError<'caller>(
    mut env: jni::EnvUnowned<'caller>,
    _class: jni::objects::JClass<'caller>,
    message: JString<'caller>,
) {
    let outcome = env.with_env(|env| {
        let message = message
            .try_to_string(env)
            .unwrap_or_else(|_| "Android 文件选择器发生错误".to_owned());
        push_notification(message, true);
        Ok::<_, jni::errors::Error>(())
    });
    outcome.resolve::<jni::errors::ThrowRuntimeExAndDefault>();
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_fasttran_app_FastTranActivity_nativeOnSafeInsets<'caller>(
    mut env: jni::EnvUnowned<'caller>,
    _class: jni::objects::JClass<'caller>,
    top: jni::sys::jint,
    right: jni::sys::jint,
    bottom: jni::sys::jint,
    left: jni::sys::jint,
) {
    let outcome = env.with_env(|_env| {
        set_safe_insets(top as f32, right as f32, bottom as f32, left as f32);
        Ok::<_, jni::errors::Error>(())
    });
    outcome.resolve::<jni::errors::ThrowRuntimeExAndDefault>();
}
