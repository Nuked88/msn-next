use chacha20poly1305::{
    aead::{rand_core::RngCore, Aead, OsRng},
    KeyInit, XChaCha20Poly1305, XNonce,
};
use msnnext_protocol::{AttachmentChunk, AttachmentManifest};
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub const CHUNK_SIZE: usize = 256 * 1024;
pub const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const MAX_PREVIEW_BYTES: u64 = 50 * 1024 * 1024;

pub fn build_manifest(path: &Path) -> Result<AttachmentManifest, Box<dyn Error>> {
    let size = fs::metadata(path)?.len();
    if size > MAX_FILE_BYTES {
        return Err(format!(
            "file oltre il limite di {} MB",
            MAX_FILE_BYTES / 1024 / 1024
        )
        .into());
    }
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("nome file non valido")?
        .to_owned();
    let mime = match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
    .to_owned();
    let mut file = File::open(path)?;
    let mut full_hash = blake3::Hasher::new();
    let mut hashes = Vec::new();
    let mut buffer = vec![0; CHUNK_SIZE];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        full_hash.update(&buffer[..read]);
        hashes.push(*blake3::hash(&buffer[..read]).as_bytes());
    }
    Ok(AttachmentManifest {
        attachment_id: *full_hash.finalize().as_bytes(),
        filename,
        mime,
        size,
        chunk_size: CHUNK_SIZE as u32,
        chunks: hashes,
    })
}

pub fn read_chunk(
    path: &Path,
    manifest: &AttachmentManifest,
    index: u32,
) -> Result<AttachmentChunk, Box<dyn Error>> {
    let expected = manifest
        .chunks
        .get(index as usize)
        .ok_or("indice chunk non valido")?;
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(index as u64 * manifest.chunk_size as u64))?;
    let mut bytes = vec![0; manifest.chunk_size as usize];
    let read = file.read(&mut bytes)?;
    bytes.truncate(read);
    if blake3::hash(&bytes).as_bytes() != expected {
        return Err("il file è cambiato durante l'invio".into());
    }
    Ok(AttachmentChunk {
        attachment_id: manifest.attachment_id,
        index,
        bytes,
    })
}

pub struct Receiver {
    root: PathBuf,
    active: HashMap<[u8; 32], ActiveTransfer>,
    cipher: XChaCha20Poly1305,
}

struct ActiveTransfer {
    manifest: AttachmentManifest,
    missing: HashSet<u32>,
}

pub struct CompletedAttachment {
    pub id: [u8; 32],
    pub filename: String,
    pub mime: String,
}

impl Receiver {
    pub fn new(root: PathBuf, key: [u8; 32]) -> Self {
        let receiver = Self {
            root,
            active: HashMap::new(),
            cipher: XChaCha20Poly1305::new((&key).into()),
        };
        receiver.seal_legacy_files();
        receiver
    }

    pub fn accept_offer(
        &mut self,
        manifest: AttachmentManifest,
    ) -> Result<(Vec<u32>, Option<CompletedAttachment>), Box<dyn Error>> {
        validate_manifest(&manifest)?;
        if self
            .load_manifest(&manifest.attachment_id)
            .is_ok_and(|stored| stored == manifest)
        {
            return Ok((vec![], Some(completed(&manifest))));
        }
        if let Ok(bytes) = self.read_legacy(&manifest.attachment_id) {
            if bytes.len() as u64 == manifest.size
                && blake3::hash(&bytes).as_bytes() == &manifest.attachment_id
            {
                return Ok((vec![], Some(completed(&manifest))));
            }
        }
        fs::create_dir_all(self.parts_dir(&manifest.attachment_id))?;
        let missing: Vec<u32> = manifest
            .chunks
            .iter()
            .enumerate()
            .filter_map(|(index, expected)| {
                let path = self.chunk_path(&manifest.attachment_id, index as u32);
                match fs::read(path)
                    .ok()
                    .and_then(|bytes| self.decrypt(&bytes).ok())
                {
                    Some(bytes) if blake3::hash(&bytes).as_bytes() == expected => None,
                    _ => Some(index as u32),
                }
            })
            .collect();
        if missing.is_empty() {
            self.finalize(&manifest)?;
            return Ok((missing, Some(completed(&manifest))));
        }
        self.active.insert(
            manifest.attachment_id,
            ActiveTransfer {
                manifest,
                missing: missing.iter().copied().collect(),
            },
        );
        Ok((missing, None))
    }

    pub fn accept_chunk(
        &mut self,
        chunk: &AttachmentChunk,
    ) -> Result<Option<CompletedAttachment>, Box<dyn Error>> {
        let manifest = self
            .active
            .get(&chunk.attachment_id)
            .ok_or("manifest mancante")?
            .manifest
            .clone();
        let expected = manifest
            .chunks
            .get(chunk.index as usize)
            .ok_or("indice chunk non valido")?;
        if chunk.bytes.len() > manifest.chunk_size as usize
            || blake3::hash(&chunk.bytes).as_bytes() != expected
        {
            return Err("chunk non valido".into());
        }
        fs::write(
            self.chunk_path(&chunk.attachment_id, chunk.index),
            self.encrypt(&chunk.bytes)?,
        )?;
        let complete = self
            .active
            .get_mut(&chunk.attachment_id)
            .is_some_and(|transfer| {
                transfer.missing.remove(&chunk.index);
                transfer.missing.is_empty()
            });
        if complete {
            self.finalize(&manifest)?;
            self.active.remove(&chunk.attachment_id);
            return Ok(Some(completed(&manifest)));
        }
        Ok(None)
    }

    fn finalize(&self, manifest: &AttachmentManifest) -> Result<(), Box<dyn Error>> {
        fs::create_dir_all(&self.root)?;
        let parts = self.parts_dir(&manifest.attachment_id);
        let mut size = 0_u64;
        let mut hash = blake3::Hasher::new();
        for index in 0..manifest.chunks.len() {
            let encrypted = fs::read(self.chunk_path(&manifest.attachment_id, index as u32))?;
            let bytes = self.decrypt(&encrypted)?;
            size += bytes.len() as u64;
            hash.update(&bytes);
        }
        if size != manifest.size || hash.finalize().as_bytes() != &manifest.attachment_id {
            return Err("file ricostruito non valido".into());
        }
        let metadata = cbor4ii::serde::to_vec(Vec::new(), manifest)?;
        fs::write(parts.join("manifest.cbor"), self.encrypt(&metadata)?)?;
        let completed = self.completed_dir(&manifest.attachment_id);
        fs::remove_dir_all(&completed).ok();
        fs::rename(parts, completed)?;
        Ok(())
    }

    fn parts_dir(&self, id: &[u8; 32]) -> PathBuf {
        self.root
            .join(".parts")
            .join(blake3::Hash::from_bytes(*id).to_hex().as_str())
    }
    fn chunk_path(&self, id: &[u8; 32], index: u32) -> PathBuf {
        self.parts_dir(id).join(format!("{index}.part"))
    }

    fn completed_dir(&self, id: &[u8; 32]) -> PathBuf {
        self.root
            .join(blake3::Hash::from_bytes(*id).to_hex().as_str())
    }

    fn target_path_for_id(&self, id: &[u8; 32]) -> PathBuf {
        let id = blake3::Hash::from_bytes(*id).to_hex().to_string();
        self.root.join(format!("{id}.vault"))
    }

    pub fn read(&self, id: &[u8; 32]) -> Result<Vec<u8>, Box<dyn Error>> {
        if let Ok(bytes) = self.read_legacy(id) {
            return Ok(bytes);
        }
        let manifest = self.load_manifest(id)?;
        if manifest.size > MAX_PREVIEW_BYTES {
            return Err("anteprima limitata a 50 MB; esporta il file per aprirlo".into());
        }
        let mut output = Vec::with_capacity(manifest.size as usize);
        for index in 0..manifest.chunks.len() {
            let path = self.completed_dir(id).join(format!("{index}.part"));
            output.extend(self.decrypt(&fs::read(path)?)?);
        }
        Ok(output)
    }

    pub fn export(&self, id: &[u8; 32], path: &Path) -> Result<(), Box<dyn Error>> {
        if let Ok(bytes) = self.read_legacy(id) {
            fs::write(path, bytes)?;
            return Ok(());
        }
        let manifest = self.load_manifest(id)?;
        let mut output = File::create(path)?;
        for index in 0..manifest.chunks.len() {
            let part = self.completed_dir(id).join(format!("{index}.part"));
            output.write_all(&self.decrypt(&fs::read(part)?)?)?;
        }
        output.flush()?;
        Ok(())
    }

    fn read_legacy(&self, id: &[u8; 32]) -> Result<Vec<u8>, Box<dyn Error>> {
        self.decrypt(&fs::read(self.target_path_for_id(id))?)
    }

    fn load_manifest(&self, id: &[u8; 32]) -> Result<AttachmentManifest, Box<dyn Error>> {
        let encrypted = fs::read(self.completed_dir(id).join("manifest.cbor"))?;
        Ok(cbor4ii::serde::from_slice(&self.decrypt(&encrypted)?)?)
    }

    fn encrypt(&self, bytes: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
        let mut nonce = [0; 24];
        OsRng.fill_bytes(&mut nonce);
        let mut encrypted = nonce.to_vec();
        encrypted.extend(
            self.cipher
                .encrypt(XNonce::from_slice(&nonce), bytes)
                .map_err(|_| "cifratura allegato fallita")?,
        );
        Ok(encrypted)
    }

    fn decrypt(&self, bytes: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
        if bytes.len() < 24 {
            return Err("allegato cifrato non valido".into());
        }
        self.cipher
            .decrypt(XNonce::from_slice(&bytes[..24]), &bytes[24..])
            .map_err(|_| "allegato non decifrabile".into())
    }

    fn seal_legacy_files(&self) {
        if let Ok(directories) = fs::read_dir(self.root.join(".parts")) {
            for directory in directories.flatten() {
                let Ok(parts) = fs::read_dir(directory.path()) else {
                    continue;
                };
                for part in parts.flatten() {
                    let path = part.path();
                    let Ok(bytes) = fs::read(&path) else { continue };
                    if self.decrypt(&bytes).is_err() {
                        if let Ok(encrypted) = self.encrypt(&bytes) {
                            fs::write(path, encrypted).ok();
                        }
                    }
                }
            }
        }
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().is_some_and(|ext| ext == "vault") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            let id = *blake3::hash(&bytes).as_bytes();
            let Ok(encrypted) = self.encrypt(&bytes) else {
                continue;
            };
            if fs::write(self.target_path_for_id(&id), encrypted).is_ok() {
                fs::remove_file(path).ok();
            }
        }
    }
}

fn completed(manifest: &AttachmentManifest) -> CompletedAttachment {
    CompletedAttachment {
        id: manifest.attachment_id,
        filename: manifest.filename.clone(),
        mime: manifest.mime.clone(),
    }
}

fn validate_manifest(manifest: &AttachmentManifest) -> Result<(), Box<dyn Error>> {
    if manifest.size > MAX_FILE_BYTES || manifest.chunk_size as usize != CHUNK_SIZE {
        return Err("manifest fuori dai limiti".into());
    }
    let expected_chunks = if manifest.size == 0 {
        0
    } else {
        manifest.size.div_ceil(manifest.chunk_size as u64) as usize
    };
    if manifest.chunks.len() != expected_chunks {
        return Err("numero chunk non valido".into());
    }
    if manifest.filename.is_empty()
        || manifest.filename.len() > 200
        || manifest.mime.len() > 100
        || manifest.mime.contains(['\t', '\r', '\n'])
    {
        return Err("metadati file non validi".into());
    }
    if !matches!(
        manifest.mime.as_str(),
        "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "video/mp4"
            | "video/webm"
            | "application/octet-stream"
    ) {
        return Err("tipo file non consentito".into());
    }
    if Path::new(&manifest.filename)
        .file_name()
        .and_then(|name| name.to_str())
        != Some(&manifest.filename)
    {
        return Err("nome file non sicuro".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_transfer_resumes_only_missing_chunks() {
        let root = std::env::temp_dir().join(format!("msnnext-attachments-{}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.bin");
        fs::write(&source, vec![7; CHUNK_SIZE + 10]).unwrap();
        let manifest = build_manifest(&source).unwrap();
        let mut receiver = Receiver::new(root.join("received"), [11; 32]);
        assert_eq!(
            receiver.accept_offer(manifest.clone()).unwrap().0,
            vec![0, 1]
        );
        receiver
            .accept_chunk(&read_chunk(&source, &manifest, 0).unwrap())
            .unwrap();

        let mut resumed = Receiver::new(root.join("received"), [11; 32]);
        assert_eq!(resumed.accept_offer(manifest.clone()).unwrap().0, vec![1]);
        let completed = resumed
            .accept_chunk(&read_chunk(&source, &manifest, 1).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            resumed.read(&completed.id).unwrap(),
            fs::read(&source).unwrap()
        );
        assert!(
            !fs::read(resumed.completed_dir(&completed.id).join("0.part"))
                .unwrap()
                .windows(16)
                .any(|window| window == [7; 16])
        );
        let exported = root.join("exported.bin");
        resumed.export(&completed.id, &exported).unwrap();
        assert_eq!(fs::read(exported).unwrap(), fs::read(source).unwrap());
        let mut duplicate = Receiver::new(root.join("received"), [11; 32]);
        let (missing, completed) = duplicate.accept_offer(manifest).unwrap();
        assert!(missing.is_empty() && completed.is_some());
        fs::remove_dir_all(root).ok();
    }
}
