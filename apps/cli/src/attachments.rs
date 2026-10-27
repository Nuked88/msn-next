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
/// Trasferimenti in arrivo tenuti in memoria allo stesso tempo: oltre questo
/// numero le nuove offerte vengono rifiutate finché uno non finisce.
pub const MAX_ACTIVE_INCOMING: usize = 32;

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
    vault: Vault,
    active: HashMap<[u8; 32], ActiveTransfer>,
}

/// Percorsi e chiave del vault cifrato. Clonabile, così il lavoro pesante
/// (verifica di un file da GB) può girare fuori dal loop di rete.
#[derive(Clone)]
pub struct Vault {
    root: PathBuf,
    cipher: XChaCha20Poly1305,
}

/// Un file arrivato per intero va ricostruito e verificato prima di finire
/// nel vault: su file grandi richiede secondi, quindi si esegue con
/// `run()` in un thread bloccante, non nel loop.
pub struct FinalizeJob {
    vault: Vault,
    manifest: AttachmentManifest,
}

impl FinalizeJob {
    pub fn attachment_id(&self) -> [u8; 32] {
        self.manifest.attachment_id
    }

    pub fn run(self) -> Result<CompletedAttachment, Box<dyn Error>> {
        if let Err(error) = self.vault.finalize(&self.manifest) {
            // Parti corrotte: le scartiamo, un nuovo invio riparte da zero.
            fs::remove_dir_all(self.vault.parts_dir(&self.manifest.attachment_id)).ok();
            return Err(error);
        }
        Ok(completed(&self.manifest))
    }
}

/// Esito di un'offerta o di un chunk: niente da fare, file già nel vault,
/// oppure file completo da verificare con un `FinalizeJob`.
pub enum Completion {
    Ready(CompletedAttachment),
    Finalize(FinalizeJob),
}

/// Riferimento a un file inviato da noi: niente copia nel vault (raddoppiava
/// lo spazio su disco), ma il file resta riapribile finché l'originale esiste.
#[derive(serde::Serialize, serde::Deserialize)]
struct LocalReference {
    manifest: AttachmentManifest,
    path: PathBuf,
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
            vault: Vault {
                root,
                cipher: XChaCha20Poly1305::new((&key).into()),
            },
            active: HashMap::new(),
        };
        receiver.vault.seal_legacy_files();
        receiver
    }

    pub fn accept_offer(
        &mut self,
        manifest: AttachmentManifest,
    ) -> Result<(Vec<u32>, Option<Completion>), Box<dyn Error>> {
        validate_manifest(&manifest)?;
        if self
            .vault
            .load_manifest(&manifest.attachment_id)
            .is_ok_and(|stored| stored == manifest)
        {
            return Ok((vec![], Some(Completion::Ready(completed(&manifest)))));
        }
        if let Ok(bytes) = self.vault.read_legacy(&manifest.attachment_id) {
            if bytes.len() as u64 == manifest.size
                && blake3::hash(&bytes).as_bytes() == &manifest.attachment_id
            {
                return Ok((vec![], Some(Completion::Ready(completed(&manifest)))));
            }
        }
        if !self.active.contains_key(&manifest.attachment_id)
            && self.active.len() >= MAX_ACTIVE_INCOMING
        {
            return Err("troppi trasferimenti in arrivo".into());
        }
        fs::create_dir_all(self.vault.parts_dir(&manifest.attachment_id))?;
        // Per il resume basta guardare la dimensione delle parti già scritte:
        // ogni chunk è stato verificato prima di salvarlo e la cifratura
        // autenticata rileva qualunque alterazione alla ricostruzione finale.
        // Rileggere e decifrare tutto qui bloccava il loop su file da GB.
        let missing: Vec<u32> = (0..manifest.chunks.len() as u32)
            .filter(|index| {
                let path = self.vault.chunk_path(&manifest.attachment_id, *index);
                fs::metadata(path).ok().map(|metadata| metadata.len())
                    != Some(encrypted_chunk_len(&manifest, *index))
            })
            .collect();
        if missing.is_empty() {
            return Ok((
                missing,
                Some(Completion::Finalize(FinalizeJob {
                    vault: self.vault.clone(),
                    manifest,
                })),
            ));
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
    ) -> Result<Option<FinalizeJob>, Box<dyn Error>> {
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
            self.vault.chunk_path(&chunk.attachment_id, chunk.index),
            self.vault.encrypt(&chunk.bytes)?,
        )?;
        let complete = self
            .active
            .get_mut(&chunk.attachment_id)
            .is_some_and(|transfer| {
                transfer.missing.remove(&chunk.index);
                transfer.missing.is_empty()
            });
        if complete {
            self.active.remove(&chunk.attachment_id);
            return Ok(Some(FinalizeJob {
                vault: self.vault.clone(),
                manifest,
            }));
        }
        Ok(None)
    }

    /// Registra un file che abbiamo inviato, senza copiarlo.
    pub fn store_reference(
        &self,
        manifest: &AttachmentManifest,
        path: &Path,
    ) -> Result<(), Box<dyn Error>> {
        let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let reference = LocalReference {
            manifest: manifest.clone(),
            path,
        };
        let bytes = cbor4ii::serde::to_vec(Vec::new(), &reference)?;
        fs::create_dir_all(self.vault.references_dir())?;
        fs::write(
            self.vault.reference_path(&manifest.attachment_id),
            self.vault.encrypt(&bytes)?,
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn read(&self, id: &[u8; 32]) -> Result<Vec<u8>, Box<dyn Error>> {
        self.vault.read(id)
    }

    /// Il mittente ha annullato: niente più chunk per questo file, e le
    /// parti già ricevute vengono eliminate.
    pub fn cancel(&mut self, id: &[u8; 32]) {
        self.active.remove(id);
        let _ = fs::remove_dir_all(self.vault.parts_dir(id));
    }

    /// Il vault da usare fuori dal loop (thread bloccanti).
    pub fn vault(&self) -> Vault {
        self.vault.clone()
    }

    pub fn export(&self, id: &[u8; 32], path: &Path) -> Result<(), Box<dyn Error>> {
        self.vault.export(id, path)
    }

    #[cfg(test)]
    fn completed_dir(&self, id: &[u8; 32]) -> PathBuf {
        self.vault.completed_dir(id)
    }
}

/// Dimensione su disco della parte cifrata `index`: nonce + dati + tag.
fn encrypted_chunk_len(manifest: &AttachmentManifest, index: u32) -> u64 {
    let start = index as u64 * manifest.chunk_size as u64;
    let len = (manifest.size - start).min(manifest.chunk_size as u64);
    24 + len + 16
}

impl Vault {
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

    fn references_dir(&self) -> PathBuf {
        self.root.join(".sent")
    }

    fn reference_path(&self, id: &[u8; 32]) -> PathBuf {
        self.references_dir()
            .join(format!("{}.ref", blake3::Hash::from_bytes(*id).to_hex()))
    }

    fn load_reference(&self, id: &[u8; 32]) -> Result<LocalReference, Box<dyn Error>> {
        let encrypted = fs::read(self.reference_path(id))?;
        Ok(cbor4ii::serde::from_slice(&self.decrypt(&encrypted)?)?)
    }

    /// Apre il file originale di un nostro invio, controllando che non sia
    /// cambiato da allora.
    fn open_reference(&self, id: &[u8; 32]) -> Result<(File, AttachmentManifest), Box<dyn Error>> {
        let reference = self
            .load_reference(id)
            .map_err(|_| "file non trovato nell'archivio")?;
        let file = File::open(&reference.path)
            .map_err(|_| "il file originale inviato non è più disponibile")?;
        if file.metadata()?.len() != reference.manifest.size {
            return Err("il file originale inviato è stato modificato".into());
        }
        Ok((file, reference.manifest))
    }

    pub fn read(&self, id: &[u8; 32]) -> Result<Vec<u8>, Box<dyn Error>> {
        if let Ok(bytes) = self.read_legacy(id) {
            return Ok(bytes);
        }
        let manifest = match self.load_manifest(id) {
            Ok(manifest) => manifest,
            Err(_) => {
                let (mut file, manifest) = self.open_reference(id)?;
                if manifest.size > MAX_PREVIEW_BYTES {
                    return Err("anteprima limitata a 50 MB; esporta il file per aprirlo".into());
                }
                let mut bytes = Vec::with_capacity(manifest.size as usize);
                file.read_to_end(&mut bytes)?;
                if blake3::hash(&bytes).as_bytes() != id {
                    return Err("il file originale inviato è stato modificato".into());
                }
                return Ok(bytes);
            }
        };
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

    fn export(&self, id: &[u8; 32], path: &Path) -> Result<(), Box<dyn Error>> {
        if let Ok(bytes) = self.read_legacy(id) {
            fs::write(path, bytes)?;
            return Ok(());
        }
        let manifest = match self.load_manifest(id) {
            Ok(manifest) => manifest,
            Err(_) => return self.export_reference(id, path),
        };
        let mut output = File::create(path)?;
        for index in 0..manifest.chunks.len() {
            let part = self.completed_dir(id).join(format!("{index}.part"));
            output.write_all(&self.decrypt(&fs::read(part)?)?)?;
        }
        output.flush()?;
        Ok(())
    }

    fn export_reference(&self, id: &[u8; 32], path: &Path) -> Result<(), Box<dyn Error>> {
        let (mut source, _) = self.open_reference(id)?;
        let mut output = File::create(path)?;
        let mut hash = blake3::Hasher::new();
        let mut buffer = vec![0; CHUNK_SIZE];
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hash.update(&buffer[..read]);
            output.write_all(&buffer[..read])?;
        }
        output.flush()?;
        if hash.finalize().as_bytes() != id {
            drop(output);
            fs::remove_file(path).ok();
            return Err("il file originale inviato è stato modificato".into());
        }
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

pub(crate) fn validate_manifest(manifest: &AttachmentManifest) -> Result<(), Box<dyn Error>> {
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
        assert!(receiver
            .accept_chunk(&read_chunk(&source, &manifest, 0).unwrap())
            .unwrap()
            .is_none());

        let mut resumed = Receiver::new(root.join("received"), [11; 32]);
        assert_eq!(resumed.accept_offer(manifest.clone()).unwrap().0, vec![1]);
        let completed = resumed
            .accept_chunk(&read_chunk(&source, &manifest, 1).unwrap())
            .unwrap()
            .unwrap()
            .run()
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
        assert!(missing.is_empty() && matches!(completed, Some(Completion::Ready(_))));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn active_incoming_transfers_are_capped() {
        let root = std::env::temp_dir().join(format!("msnnext-cap-{}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(&root).unwrap();
        let mut receiver = Receiver::new(root.join("received"), [12; 32]);
        let manifests: Vec<_> = (0..=MAX_ACTIVE_INCOMING)
            .map(|index| {
                let source = root.join(format!("{index}.bin"));
                fs::write(&source, vec![index as u8; CHUNK_SIZE + 1]).unwrap();
                build_manifest(&source).unwrap()
            })
            .collect();
        for manifest in &manifests[..MAX_ACTIVE_INCOMING] {
            receiver.accept_offer(manifest.clone()).unwrap();
        }
        assert!(receiver
            .accept_offer(manifests[MAX_ACTIVE_INCOMING].clone())
            .is_err());
        // Riproporre un trasferimento già attivo (resume) resta possibile.
        assert!(receiver.accept_offer(manifests[0].clone()).is_ok());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn corrupted_part_fails_finalize_and_restarts_transfer() {
        let root = std::env::temp_dir().join(format!("msnnext-corrupt-{}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.bin");
        fs::write(&source, vec![3; CHUNK_SIZE + 10]).unwrap();
        let manifest = build_manifest(&source).unwrap();
        let mut receiver = Receiver::new(root.join("received"), [12; 32]);
        receiver.accept_offer(manifest.clone()).unwrap();
        receiver
            .accept_chunk(&read_chunk(&source, &manifest, 0).unwrap())
            .unwrap();
        // Una parte alterata su disco con la stessa dimensione.
        let part = receiver.vault.chunk_path(&manifest.attachment_id, 0);
        let mut bytes = fs::read(&part).unwrap();
        bytes[40] ^= 1;
        fs::write(&part, bytes).unwrap();
        let job = receiver
            .accept_chunk(&read_chunk(&source, &manifest, 1).unwrap())
            .unwrap()
            .unwrap();
        assert!(job.run().is_err());
        assert_eq!(receiver.accept_offer(manifest).unwrap().0, vec![0, 1]);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn sent_file_is_referenced_not_copied() {
        let root = std::env::temp_dir().join(format!("msnnext-sent-{}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(&root).unwrap();
        let source = root.join("foto.png");
        fs::write(&source, vec![5; 3 * CHUNK_SIZE]).unwrap();
        let manifest = build_manifest(&source).unwrap();
        let receiver = Receiver::new(root.join("vault"), [13; 32]);
        receiver.store_reference(&manifest, &source).unwrap();

        let vault_bytes: u64 = walk_size(&root.join("vault"));
        assert!(
            vault_bytes < 4096,
            "il vault non deve contenere una copia: {vault_bytes}"
        );
        assert_eq!(
            receiver.read(&manifest.attachment_id).unwrap(),
            fs::read(&source).unwrap()
        );
        let exported = root.join("copia.png");
        receiver.export(&manifest.attachment_id, &exported).unwrap();
        assert_eq!(fs::read(&exported).unwrap(), fs::read(&source).unwrap());

        // Se l'originale cambia, non si spaccia un altro file per quello inviato.
        fs::write(&source, vec![6; 3 * CHUNK_SIZE]).unwrap();
        assert!(receiver.read(&manifest.attachment_id).is_err());
        assert!(receiver
            .export(&manifest.attachment_id, &root.join("altra.png"))
            .is_err());
        assert!(!root.join("altra.png").exists());
        fs::remove_dir_all(root).ok();
    }

    fn walk_size(path: &Path) -> u64 {
        fs::read_dir(path)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| {
                        let path = entry.path();
                        if path.is_dir() {
                            walk_size(&path)
                        } else {
                            entry.metadata().map(|metadata| metadata.len()).unwrap_or(0)
                        }
                    })
                    .sum()
            })
            .unwrap_or(0)
    }
}
