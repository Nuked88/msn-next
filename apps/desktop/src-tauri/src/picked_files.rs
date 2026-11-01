//! Su Android i dialog file restituiscono URI `content://`, non percorsi:
//! std::fs (qui e nel core) fallisce con "No such file or directory".
//! I file scelti vengono copiati nella cache dell'app e il resto del codice
//! lavora sempre con percorsi veri. Per i salvataggi si scrive su un file in
//! cache e lo si copia sull'URI scelto a scrittura finita (`finish_save`).

use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};

use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_fs::{FilePath, FsExt, OpenOptions};

const PICKED_DIR: &str = "picked-files";
const SAVES_DIR: &str = "pending-saves";

#[derive(Default)]
pub struct PendingSaves {
    targets: Mutex<HashMap<String, FilePath>>,
    next: AtomicU64,
}

/// Svuota le copie lasciate dalle sessioni precedenti.
pub fn cleanup<R: Runtime>(app: &AppHandle<R>) {
    if let Ok(cache) = app.path().app_cache_dir() {
        let _ = fs::remove_dir_all(cache.join(PICKED_DIR));
        let _ = fs::remove_dir_all(cache.join(SAVES_DIR));
    }
}

fn unique_dir<R: Runtime>(app: &AppHandle<R>, kind: &str) -> Result<PathBuf, String> {
    let saves = app.state::<PendingSaves>();
    let id = saves.next.fetch_add(1, Ordering::Relaxed);
    let dir = app
        .path()
        .app_cache_dir()
        .map_err(|error| error.to_string())?
        .join(kind)
        .join(format!("{}-{id}", std::process::id()));
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    Ok(dir)
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Percorso locale per un file scelto dall'utente: copia gli URI in cache.
pub fn local_copy<R: Runtime>(app: &AppHandle<R>, picked: FilePath) -> Result<String, String> {
    let uri = match picked {
        FilePath::Path(path) => return Ok(path_string(&path)),
        FilePath::Url(url) if url.scheme() == "file" => {
            return url
                .to_file_path()
                .map(|path| path_string(&path))
                .map_err(|_| "invalid file path".to_owned())
        }
        FilePath::Url(url) => url,
    };
    let name = name_from_uri(uri.as_str());
    let dir = unique_dir(app, PICKED_DIR)?;
    let copy = dir.join(&name);
    let read_error = |error: io::Error| format!("file could not be read: {error}");
    let mut input = app
        .fs()
        .open(FilePath::Url(uri), OpenOptions::new().read(true).clone())
        .map_err(read_error)?;
    let mut output = fs::File::create(&copy).map_err(read_error)?;
    io::copy(&mut input, &mut output).map_err(read_error)?;
    drop(output);
    // Gli URI dei media ("image:1234") non hanno estensione: la si ricava dal
    // contenuto, così il destinatario vede il tipo giusto.
    if Path::new(&name).extension().is_none() {
        if let Ok(Some(kind)) = infer::get_from_path(&copy) {
            let renamed = dir.join(format!("{name}.{}", kind.extension()));
            if fs::rename(&copy, &renamed).is_ok() {
                return Ok(path_string(&renamed));
            }
        }
    }
    Ok(path_string(&copy))
}

/// Percorso locale su cui scrivere un salvataggio; se l'utente ha scelto un
/// URI, la copia avviene in `finish_save`.
pub fn save_target<R: Runtime>(
    app: &AppHandle<R>,
    picked: FilePath,
    default_name: Option<&str>,
) -> Result<String, String> {
    let uri = match picked {
        FilePath::Path(path) => return Ok(path_string(&path)),
        FilePath::Url(url) if url.scheme() == "file" => {
            return url
                .to_file_path()
                .map(|path| path_string(&path))
                .map_err(|_| "invalid file path".to_owned())
        }
        FilePath::Url(url) => url,
    };
    let name = sanitize_name(default_name.unwrap_or_default());
    let target = path_string(&unique_dir(app, SAVES_DIR)?.join(name));
    app.state::<PendingSaves>()
        .targets
        .lock()
        .map_err(|_| "save state unavailable".to_owned())?
        .insert(target.clone(), FilePath::Url(uri));
    Ok(target)
}

/// Dopo aver scritto `path`: se era un salvataggio verso un URI, lo copia lì.
pub fn finish_save<R: Runtime>(app: &AppHandle<R>, path: &str) -> Result<(), String> {
    let Some(uri) = app
        .state::<PendingSaves>()
        .targets
        .lock()
        .map_err(|_| "save state unavailable".to_owned())?
        .remove(path)
    else {
        return Ok(());
    };
    let save_error = |error: io::Error| format!("file could not be saved: {error}");
    let mut input = fs::File::open(path).map_err(save_error)?;
    let mut output = app
        .fs()
        .open(
            uri,
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .clone(),
        )
        .map_err(save_error)?;
    io::copy(&mut input, &mut output).map_err(save_error)?;
    if let Some(dir) = Path::new(path).parent() {
        let _ = fs::remove_dir_all(dir);
    }
    Ok(())
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(byte) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_owned();
    if cleaned.is_empty() {
        "file".into()
    } else {
        cleaned
    }
}

/// Nome leggibile dall'ultimo segmento dell'URI: "…/image%3A1234" diventa
/// "image-1234", "…/primary%3ADownload%2Fdoc.pdf" diventa "doc.pdf".
pub fn name_from_uri(uri: &str) -> String {
    let segment = uri.split(['?', '#']).next().unwrap_or_default();
    let segment = segment.rsplit('/').next().unwrap_or_default();
    let decoded = percent_decode(segment);
    let mut name = decoded.rsplit('/').next().unwrap_or_default();
    if let Some((_, rest)) = name.rsplit_once(':') {
        if rest.contains('.') {
            name = rest;
        }
    }
    sanitize_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_uri_names_are_readable() {
        assert_eq!(
            name_from_uri(
                "content://com.android.providers.media.documents/document/image%3A116420"
            ),
            "image-116420"
        );
        assert_eq!(
            name_from_uri("content://com.android.externalstorage.documents/document/primary%3ADownload%2Freport.pdf"),
            "report.pdf"
        );
        assert_eq!(
            name_from_uri("content://media/external/file/..%2F..%2Fetc"),
            "etc"
        );
        assert_eq!(name_from_uri("content://x/document/%2E%2E"), "file");
        assert_eq!(name_from_uri("content://x/doc/a%2"), "a-2");
    }

    #[test]
    fn local_paths_pass_through() {
        assert_eq!(sanitize_name("../photo.png"), "-photo.png");
        assert_eq!(sanitize_name(""), "file");
    }
}
