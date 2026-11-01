//! Aggiornamenti dell'APK Android.
//!
//! Su mobile il plugin updater sa controllare latest.json e scaricare (con
//! verifica della firma minisign), ma `install` non fa nulla. Qui si usa la
//! voce `android-aarch64` di latest.json, si scarica l'APK verificato nella
//! cache dell'app e lo si passa all'installer di sistema tramite FileProvider.
//! Android stesso accetta l'aggiornamento solo se l'APK è firmato con la
//! stessa chiave dell'app installata.

use std::{fs, path::PathBuf, sync::Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, Runtime, State};
use tauri_plugin_updater::{Update, UpdaterExt};

/// Chiave della piattaforma in latest.json (vedi build-release.yml).
pub const ANDROID_TARGET: &str = "android-aarch64";
const UPDATES_DIR: &str = "apk-updates";
const PROGRESS_EVENT: &str = "android-update-progress";

#[derive(Default)]
pub struct PendingUpdate(Mutex<Option<Update>>);

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableUpdate {
    pub version: String,
    pub current_version: String,
    pub notes: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Progress {
    downloaded: u64,
    total: Option<u64>,
}

/// Elimina gli APK scaricati in precedenza (già installati o abbandonati).
pub fn cleanup<R: Runtime>(app: &AppHandle<R>) {
    if let Ok(cache) = app.path().app_cache_dir() {
        let _ = fs::remove_dir_all(cache.join(UPDATES_DIR));
    }
}

#[tauri::command]
pub async fn android_update_check<R: Runtime>(
    app: AppHandle<R>,
    pending: State<'_, PendingUpdate>,
) -> Result<Option<AvailableUpdate>, String> {
    // Su Android non c'è un eseguibile da sostituire: un percorso qualsiasi
    // evita che il builder dipenda da current_exe().
    let placeholder = app
        .path()
        .app_cache_dir()
        .map_err(|error| error.to_string())?
        .join(UPDATES_DIR)
        .join("msnnext");
    let update = app
        .updater_builder()
        .target(ANDROID_TARGET)
        .executable_path(placeholder)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| error.to_string())?
        .check()
        .await
        .map_err(|error| error.to_string())?;
    let available = update.as_ref().map(|update| AvailableUpdate {
        version: update.version.clone(),
        current_version: update.current_version.clone(),
        notes: update.body.clone().filter(|notes| !notes.trim().is_empty()),
    });
    *pending.0.lock().map_err(|error| error.to_string())? = update;
    Ok(available)
}

/// Scarica l'APK (firma verificata dal plugin) e apre l'installer di sistema.
#[tauri::command]
pub async fn android_update_install<R: Runtime>(
    app: AppHandle<R>,
    pending: State<'_, PendingUpdate>,
) -> Result<(), String> {
    let update = pending
        .0
        .lock()
        .map_err(|error| error.to_string())?
        .clone()
        .ok_or_else(|| "no update available".to_owned())?;
    let mut downloaded = 0u64;
    let progress_app = app.clone();
    let bytes = update
        .download(
            move |chunk, total| {
                downloaded += chunk as u64;
                let _ = progress_app.emit(PROGRESS_EVENT, Progress { downloaded, total });
            },
            || {},
        )
        .await
        .map_err(|error| error.to_string())?;
    let path = write_apk(&app, &update.version, &bytes)?;
    install_apk(&app, path)
}

fn write_apk<R: Runtime>(
    app: &AppHandle<R>,
    version: &str,
    bytes: &[u8],
) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_cache_dir()
        .map_err(|error| error.to_string())?
        .join(UPDATES_DIR);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let path = dir.join(apk_file_name(version));
    fs::write(&path, bytes).map_err(|error| format!("APK could not be saved: {error}"))?;
    Ok(path)
}

fn apk_file_name(version: &str) -> String {
    let version: String = version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("msnnext_{version}.apk")
}

#[cfg(target_os = "android")]
fn install_apk<R: Runtime>(app: &AppHandle<R>, path: PathBuf) -> Result<(), String> {
    let window = app
        .webview_windows()
        .into_values()
        .next()
        .ok_or_else(|| "no window".to_owned())?;
    let (sender, receiver) = std::sync::mpsc::channel();
    let path = path.to_string_lossy().into_owned();
    window
        .with_webview(move |webview| {
            webview.jni_handle().exec(move |env, activity, _webview| {
                let _ = sender.send(android::open_installer(env, activity, &path));
            });
        })
        .map_err(|error| error.to_string())?;
    receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .map_err(|_| "installer did not respond".to_owned())?
}

#[cfg(not(target_os = "android"))]
fn install_apk<R: Runtime>(_app: &AppHandle<R>, _path: PathBuf) -> Result<(), String> {
    Err("APK updates are only available on Android".to_owned())
}

#[cfg(target_os = "android")]
mod android {
    use jni::{
        objects::{JClass, JObject, JString, JValue},
        JNIEnv,
    };

    const FLAG_GRANT_READ_URI_PERMISSION: i32 = 0x0000_0001;
    const FLAG_ACTIVITY_NEW_TASK: i32 = 0x1000_0000;

    pub fn open_installer(env: &mut JNIEnv, activity: &JObject, path: &str) -> Result<(), String> {
        let result = start(env, activity, path);
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_describe();
            let _ = env.exception_clear();
        }
        result.map_err(|error| format!("installer could not be opened: {error}"))
    }

    fn start(env: &mut JNIEnv, activity: &JObject, path: &str) -> jni::errors::Result<()> {
        // content:// dal FileProvider del template Tauri (cache-path in
        // res/xml/file_paths.xml, garantito da scripts/patch-android-project.sh).
        let package = env
            .call_method(activity, "getPackageName", "()Ljava/lang/String;", &[])?
            .l()?;
        let package: String = env.get_string(&JString::from(package))?.into();
        let authority = env.new_string(format!("{package}.fileprovider"))?;
        let path = env.new_string(path)?;
        let file = env.new_object("java/io/File", "(Ljava/lang/String;)V", &[(&path).into()])?;
        // androidx è nel classloader dell'app, non in quello di sistema.
        let loader = env
            .call_method(activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?
            .l()?;
        let name = env.new_string("androidx.core.content.FileProvider")?;
        let provider = env
            .call_method(
                &loader,
                "loadClass",
                "(Ljava/lang/String;)Ljava/lang/Class;",
                &[(&name).into()],
            )?
            .l()?;
        let provider = JClass::from(provider);
        let uri = env
            .call_static_method(
                &provider,
                "getUriForFile",
                "(Landroid/content/Context;Ljava/lang/String;Ljava/io/File;)Landroid/net/Uri;",
                &[activity.into(), (&authority).into(), (&file).into()],
            )?
            .l()?;

        let action = env.new_string("android.intent.action.VIEW")?;
        let intent = env.new_object(
            "android/content/Intent",
            "(Ljava/lang/String;)V",
            &[(&action).into()],
        )?;
        let mime = env.new_string("application/vnd.android.package-archive")?;
        env.call_method(
            &intent,
            "setDataAndType",
            "(Landroid/net/Uri;Ljava/lang/String;)Landroid/content/Intent;",
            &[(&uri).into(), (&mime).into()],
        )?;
        env.call_method(
            &intent,
            "addFlags",
            "(I)Landroid/content/Intent;",
            &[JValue::Int(
                FLAG_GRANT_READ_URI_PERMISSION | FLAG_ACTIVITY_NEW_TASK,
            )],
        )?;
        // Se l'app non è ancora autorizzata a installare, l'installer di
        // sistema mostra lui il passaggio alle impostazioni e poi prosegue.
        env.call_method(
            activity,
            "startActivity",
            "(Landroid/content/Intent;)V",
            &[(&intent).into()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apk_names_are_safe() {
        assert_eq!(apk_file_name("0.8.15"), "msnnext_0.8.15.apk");
        assert_eq!(apk_file_name("../1.0/x"), "msnnext_.._1.0_x.apk");
    }
}
