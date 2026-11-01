//! Immagini che arrivano dai contatti (avatar, emoticon personalizzate).
//!
//! I byte ricevuti non vengono mai salvati né mostrati così come sono: si
//! decodificano con limiti stretti e si ricodificano da zero (PNG per le
//! immagini ferme, GIF per quelle animate). Metadati, profili colore, dati
//! accodati al file e file "poliglotti" spariscono, e un file costruito per
//! sfruttare un decoder della WebView non arriva mai alla WebView. Il
//! risultato resta in cache: la ricodifica avviene solo alla prima ricezione.

use std::error::Error;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use image::codecs::gif::{GifDecoder, GifEncoder, Repeat};
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use msnnext_protocol::Mime;

/// Lato dell'avatar inviato e mostrato: basta per ogni punto della UI.
pub const AVATAR_SIDE: u32 = 192;
/// Avatar ricevuto più grande di così: rifiutato senza decodificarlo.
pub const MAX_AVATAR_BYTES: usize = 200_000;
/// Dimensioni massime dichiarate da un avatar ricevuto, prima di decodificarlo.
const MAX_AVATAR_SOURCE_SIDE: u32 = 2048;
/// Fotogrammi massimi di un'emoticon animata ricevuta.
const MAX_ANIMATION_FRAMES: usize = 300;
/// Ricodifiche in corso al massimo: un contatto non può occupare tutti i core.
const MAX_IMAGE_JOBS: usize = 2;
/// Le emoticon ricevute e mai salvate escono dalla cache dopo un mese.
const RECEIVED_EMOTICON_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

pub struct Reencoded {
    pub bytes: Vec<u8>,
    pub mime: Mime,
    pub width: u16,
    pub height: u16,
    pub animated: bool,
}

fn limits(max_side: u32) -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(max_side);
    limits.max_image_height = Some(max_side);
    limits.max_alloc = Some(64 * 1024 * 1024);
    limits
}

fn decode_still(bytes: &[u8], max_side: u32) -> Result<DynamicImage, Box<dyn Error>> {
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    match reader.format() {
        Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP) => {}
        _ => return Err("formato immagine non supportato".into()),
    }
    reader.limits(limits(max_side));
    Ok(reader.decode()?)
}

fn encode_png(image: &DynamicImage) -> Result<(Vec<u8>, u32, u32), Box<dyn Error>> {
    // Un buffer RGBA nuovo: dell'originale restano solo i pixel.
    let rgba = image.to_rgba8();
    let mut png = Cursor::new(Vec::new());
    rgba.write_to(&mut png, ImageFormat::Png)?;
    Ok((png.into_inner(), rgba.width(), rgba.height()))
}

/// Decodifica un'immagine ferma e la ricodifica come PNG, rimpicciolita se
/// supera `side`.
pub fn reencode_still(
    bytes: &[u8],
    side: u32,
    max_source_side: u32,
) -> Result<(Vec<u8>, u32, u32), Box<dyn Error>> {
    let image = decode_still(bytes, max_source_side)?;
    let image = if image.width() > side || image.height() > side {
        image.thumbnail(side, side)
    } else {
        image
    };
    encode_png(&image)
}

/// L'avatar da inviare ai contatti, ricavato dall'immagine del profilo.
pub fn outgoing_avatar(source: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    let (png, _, _) = reencode_still(source, AVATAR_SIDE, 16_384)?;
    if png.len() > MAX_AVATAR_BYTES {
        return Err("avatar troppo grande anche dopo la compressione".into());
    }
    Ok(png)
}

/// Controlla e ricodifica un avatar ricevuto.
pub fn reencode_avatar(bytes: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    if bytes.is_empty() || bytes.len() > MAX_AVATAR_BYTES {
        return Err("avatar di dimensione non valida".into());
    }
    let size = imagesize::blob_size(bytes)?;
    if size.width == 0
        || size.height == 0
        || size.width > MAX_AVATAR_SOURCE_SIDE as usize
        || size.height > MAX_AVATAR_SOURCE_SIDE as usize
    {
        return Err("dimensioni avatar non valide".into());
    }
    let (png, _, _) = reencode_still(bytes, AVATAR_SIDE, MAX_AVATAR_SOURCE_SIDE)?;
    Ok(png)
}

/// Ricodifica un'emoticon ricevuta (già validata: hash, MIME e dimensioni).
/// Le animate tornano GIF animate, le altre PNG.
pub fn reencode_emoticon(
    bytes: &[u8],
    mime: Mime,
    animated: bool,
    max_side: u32,
) -> Result<Reencoded, Box<dyn Error>> {
    if animated && matches!(mime, Mime::Gif | Mime::Webp) {
        if let Some(reencoded) = reencode_animation(bytes, mime, max_side)? {
            return Ok(reencoded);
        }
    }
    let (png, width, height) = reencode_still(bytes, max_side, max_side)?;
    Ok(Reencoded {
        bytes: png,
        mime: Mime::Png,
        width: width as u16,
        height: height as u16,
        animated: false,
    })
}

/// `None` se l'animazione ha un solo fotogramma: basta un PNG.
fn reencode_animation(
    bytes: &[u8],
    mime: Mime,
    max_side: u32,
) -> Result<Option<Reencoded>, Box<dyn Error>> {
    let frames = match mime {
        Mime::Gif => {
            let mut decoder = GifDecoder::new(Cursor::new(bytes))?;
            decoder.set_limits(limits(max_side))?;
            decoder.into_frames()
        }
        Mime::Webp => {
            let mut decoder = WebPDecoder::new(Cursor::new(bytes))?;
            if !decoder.has_animation() {
                return Ok(None);
            }
            decoder.set_limits(limits(max_side))?;
            decoder.into_frames()
        }
        _ => return Ok(None),
    };
    let mut out = Vec::new();
    let mut count = 0;
    let mut size = (0, 0);
    {
        // Velocità massima di quantizzazione: la qualità di un'emoticon non
        // ne risente e la ricodifica resta breve.
        let mut encoder = GifEncoder::new_with_speed(&mut out, 30);
        encoder.set_repeat(Repeat::Infinite)?;
        for frame in frames {
            let frame = frame?;
            count += 1;
            if count > MAX_ANIMATION_FRAMES {
                return Err("troppi fotogrammi".into());
            }
            let buffer = frame.buffer();
            if buffer.width() > max_side || buffer.height() > max_side {
                return Err("dimensioni fotogramma non valide".into());
            }
            size = (buffer.width(), buffer.height());
            encoder.encode_frame(frame)?;
        }
    }
    if count <= 1 {
        return Ok(None);
    }
    Ok(Some(Reencoded {
        bytes: out,
        mime: Mime::Gif,
        width: size.0 as u16,
        height: size.1 as u16,
        animated: true,
    }))
}

pub fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", BASE64.encode(bytes))
}

/// Limita le ricodifiche contemporanee. Il permesso si libera quando la
/// guardia esce di scena, anche se il lavoro fallisce.
#[derive(Clone, Default)]
pub struct ImageJobs(Arc<AtomicUsize>);

pub struct ImageJob(Arc<AtomicUsize>);

impl ImageJobs {
    pub fn try_start(&self) -> Option<ImageJob> {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |running| {
                (running < MAX_IMAGE_JOBS).then_some(running + 1)
            })
            .ok()
            .map(|_| ImageJob(self.0.clone()))
    }
}

impl Drop for ImageJob {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Cache degli avatar ricodificati dei contatti: `<peer>.png` più `<peer>.id`
/// con l'hash della versione originale ricevuta.
#[derive(Clone)]
pub struct AvatarCache {
    dir: PathBuf,
}

fn safe_name(peer: &str) -> Option<&str> {
    (!peer.is_empty() && peer.len() <= 128 && peer.chars().all(|c| c.is_ascii_alphanumeric()))
        .then_some(peer)
}

impl AvatarCache {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn cached_id(&self, peer: &str) -> Option<[u8; 32]> {
        let peer = safe_name(peer)?;
        if !self.dir.join(format!("{peer}.png")).is_file() {
            return None;
        }
        let hex = fs::read_to_string(self.dir.join(format!("{peer}.id"))).ok()?;
        blake3::Hash::from_hex(hex.trim())
            .ok()
            .map(|hash| *hash.as_bytes())
    }

    pub fn store(
        &self,
        peer: &str,
        avatar_id: &[u8; 32],
        png: &[u8],
    ) -> Result<(), Box<dyn Error>> {
        let peer = safe_name(peer).ok_or("peer non valido")?;
        fs::create_dir_all(&self.dir)?;
        fs::write(self.dir.join(format!("{peer}.png")), png)?;
        fs::write(
            self.dir.join(format!("{peer}.id")),
            blake3::Hash::from_bytes(*avatar_id).to_hex().as_str(),
        )?;
        Ok(())
    }

    /// `true` se c'era un avatar da togliere.
    pub fn remove(&self, peer: &str) -> bool {
        let Some(peer) = safe_name(peer) else {
            return false;
        };
        let removed = fs::remove_file(self.dir.join(format!("{peer}.png"))).is_ok();
        fs::remove_file(self.dir.join(format!("{peer}.id"))).ok();
        removed
    }

    pub fn data_url(&self, peer: &str) -> Option<String> {
        let peer = safe_name(peer)?;
        fs::read(self.dir.join(format!("{peer}.png")))
            .ok()
            .map(|bytes| data_url("image/png", &bytes))
    }
}

/// Cache delle emoticon ricevute già ricodificate, per hash dell'originale.
pub fn received_emoticons_dir(store: &Path) -> PathBuf {
    store.join("received")
}

fn received_path(store: &Path, asset_id: &[u8; 32], mime: Mime) -> PathBuf {
    let extension = match mime {
        Mime::Gif => "gif",
        _ => "png",
    };
    received_emoticons_dir(store).join(format!(
        "{}.{extension}",
        blake3::Hash::from_bytes(*asset_id).to_hex()
    ))
}

pub fn store_received_emoticon(
    store: &Path,
    asset_id: &[u8; 32],
    reencoded: &Reencoded,
) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(received_emoticons_dir(store))?;
    fs::write(
        received_path(store, asset_id, reencoded.mime),
        &reencoded.bytes,
    )?;
    Ok(())
}

/// L'emoticon ricevuta e già ricodificata, se è in cache.
pub fn load_received_emoticon(store: &Path, asset_id: &[u8; 32]) -> Option<Reencoded> {
    [Mime::Gif, Mime::Png].into_iter().find_map(|mime| {
        let bytes = fs::read(received_path(store, asset_id, mime)).ok()?;
        let size = imagesize::blob_size(&bytes).ok()?;
        Some(Reencoded {
            animated: mime == Mime::Gif,
            width: size.width as u16,
            height: size.height as u16,
            mime,
            bytes,
        })
    })
}

/// Toglie dalla cache le emoticon ricevute più vecchie di un mese.
pub fn prune_received_emoticons(store: &Path) {
    let Ok(entries) = fs::read_dir(received_emoticons_dir(store)) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let expired = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > RECEIVED_EMOTICON_TTL);
        if expired {
            fs::remove_file(entry.path()).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Delay, Frame, Rgba, RgbaImage};

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = RgbaImage::from_pixel(width, height, Rgba([200, 30, 30, 255]));
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn avatar_is_reencoded_and_trailing_data_is_dropped() {
        let mut bytes = png(400, 300);
        // Dati accodati (es. un file "poliglotta"): non devono sopravvivere.
        bytes.extend_from_slice(b"<script>alert(1)</script>");
        let out = reencode_avatar(&bytes).unwrap();
        assert_ne!(out, bytes);
        assert!(!out.windows(8).any(|part| part == b"<script>"));
        let size = imagesize::blob_size(&out).unwrap();
        assert_eq!((size.width, size.height), (192, 144));
    }

    #[test]
    fn avatar_rejects_garbage_and_huge_dimensions() {
        assert!(reencode_avatar(b"not an image at all").is_err());
        assert!(reencode_avatar(&[]).is_err());
        // Intestazione PNG che dichiara 30000x30000: rifiutata prima del decode.
        let mut huge = png(1, 1);
        huge[16..20].copy_from_slice(&30_000u32.to_be_bytes());
        huge[20..24].copy_from_slice(&30_000u32.to_be_bytes());
        assert!(reencode_avatar(&huge).is_err());
    }

    #[test]
    fn animated_gif_stays_animated_after_reencoding() {
        let mut gif = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut gif);
            for color in [[255, 0, 0, 255], [0, 0, 255, 255]] {
                let frame = Frame::from_parts(
                    RgbaImage::from_pixel(16, 16, Rgba(color)),
                    0,
                    0,
                    Delay::from_numer_denom_ms(100, 1),
                );
                encoder.encode_frame(frame).unwrap();
            }
        }
        let out = reencode_emoticon(&gif, Mime::Gif, true, 512).unwrap();
        assert_eq!(out.mime, Mime::Gif);
        assert!(out.animated);
        assert_eq!((out.width, out.height), (16, 16));
        assert_ne!(out.bytes, gif);
        let frames = GifDecoder::new(Cursor::new(&out.bytes))
            .unwrap()
            .into_frames()
            .count();
        assert_eq!(frames, 2);
    }

    #[test]
    fn still_emoticon_becomes_png() {
        let out = reencode_emoticon(&png(32, 20), Mime::Png, false, 512).unwrap();
        assert_eq!(out.mime, Mime::Png);
        assert_eq!((out.width, out.height), (32, 20));
    }

    #[test]
    fn avatar_cache_round_trip() {
        let dir = std::env::temp_dir().join(format!("msnnext-avatars-{}", std::process::id()));
        let cache = AvatarCache::new(dir.clone());
        let id = [7u8; 32];
        assert_eq!(cache.cached_id("12D3KooWabc"), None);
        cache.store("12D3KooWabc", &id, &png(4, 4)).unwrap();
        assert_eq!(cache.cached_id("12D3KooWabc"), Some(id));
        assert!(cache
            .data_url("12D3KooWabc")
            .unwrap()
            .starts_with("data:image/png;base64,"));
        assert!(cache.store("../evil", &id, b"x").is_err());
        assert!(cache.remove("12D3KooWabc"));
        assert_eq!(cache.cached_id("12D3KooWabc"), None);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn image_jobs_are_capped() {
        let jobs = ImageJobs::default();
        let first = jobs.try_start().unwrap();
        let _second = jobs.try_start().unwrap();
        assert!(jobs.try_start().is_none());
        drop(first);
        assert!(jobs.try_start().is_some());
    }
}
