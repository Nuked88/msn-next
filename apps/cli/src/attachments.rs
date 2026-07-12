use msnnext_protocol::{AttachmentChunk, AttachmentManifest};
use std::{
    collections::HashMap,
    error::Error,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub const CHUNK_SIZE: usize = 256 * 1024;
pub const MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;

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
    active: HashMap<[u8; 32], AttachmentManifest>,
}

impl Receiver {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            active: HashMap::new(),
        }
    }

    pub fn accept_offer(
        &mut self,
        manifest: AttachmentManifest,
    ) -> Result<(Vec<u32>, Option<PathBuf>), Box<dyn Error>> {
        validate_manifest(&manifest)?;
        let target = self.target_path(&manifest);
        if let Ok(bytes) = fs::read(&target) {
            if bytes.len() as u64 == manifest.size
                && blake3::hash(&bytes).as_bytes() == &manifest.attachment_id
            {
                return Ok((vec![], Some(target)));
            }
        }
        fs::create_dir_all(self.parts_dir(&manifest.attachment_id))?;
        let missing: Vec<u32> = manifest
            .chunks
            .iter()
            .enumerate()
            .filter_map(|(index, expected)| {
                let path = self.chunk_path(&manifest.attachment_id, index as u32);
                match fs::read(path) {
                    Ok(bytes) if blake3::hash(&bytes).as_bytes() == expected => None,
                    _ => Some(index as u32),
                }
            })
            .collect();
        if missing.is_empty() {
            return Ok((missing, Some(self.assemble(&manifest)?)));
        }
        self.active.insert(manifest.attachment_id, manifest);
        Ok((missing, None))
    }

    pub fn accept_chunk(
        &mut self,
        chunk: &AttachmentChunk,
    ) -> Result<Option<PathBuf>, Box<dyn Error>> {
        let manifest = self
            .active
            .get(&chunk.attachment_id)
            .ok_or("manifest mancante")?
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
            &chunk.bytes,
        )?;
        if self.missing(&manifest).is_empty() {
            let path = self.assemble(&manifest)?;
            self.active.remove(&chunk.attachment_id);
            return Ok(Some(path));
        }
        Ok(None)
    }

    fn missing(&self, manifest: &AttachmentManifest) -> Vec<u32> {
        manifest
            .chunks
            .iter()
            .enumerate()
            .filter_map(|(index, expected)| {
                fs::read(self.chunk_path(&manifest.attachment_id, index as u32))
                    .ok()
                    .filter(|bytes| blake3::hash(bytes).as_bytes() == expected)
                    .is_none()
                    .then_some(index as u32)
            })
            .collect()
    }

    fn assemble(&self, manifest: &AttachmentManifest) -> Result<PathBuf, Box<dyn Error>> {
        fs::create_dir_all(&self.root)?;
        let id = blake3::Hash::from_bytes(manifest.attachment_id)
            .to_hex()
            .to_string();
        let target = self.target_path(manifest);
        let temporary = self.root.join(format!(".{id}.tmp"));
        let mut output = File::create(&temporary)?;
        for index in 0..manifest.chunks.len() {
            output.write_all(&fs::read(
                self.chunk_path(&manifest.attachment_id, index as u32),
            )?)?;
        }
        output.flush()?;
        drop(output);
        let bytes = fs::read(&temporary)?;
        if bytes.len() as u64 != manifest.size
            || blake3::hash(&bytes).as_bytes() != &manifest.attachment_id
        {
            fs::remove_file(&temporary).ok();
            return Err("file ricostruito non valido".into());
        }
        if target.exists() {
            fs::remove_file(&temporary)?;
        } else {
            fs::rename(&temporary, &target)?;
        }
        fs::remove_dir_all(self.parts_dir(&manifest.attachment_id)).ok();
        Ok(target)
    }

    fn parts_dir(&self, id: &[u8; 32]) -> PathBuf {
        self.root
            .join(".parts")
            .join(blake3::Hash::from_bytes(*id).to_hex().to_string())
    }
    fn chunk_path(&self, id: &[u8; 32], index: u32) -> PathBuf {
        self.parts_dir(id).join(format!("{index}.part"))
    }

    fn target_path(&self, manifest: &AttachmentManifest) -> PathBuf {
        let id = blake3::Hash::from_bytes(manifest.attachment_id)
            .to_hex()
            .to_string();
        self.root
            .join(format!("{}-{}", &id[..12], manifest.filename))
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
    if manifest.filename.is_empty() || manifest.filename.len() > 200 || manifest.mime.len() > 100 {
        return Err("metadati file non validi".into());
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
        let mut receiver = Receiver::new(root.join("received"));
        assert_eq!(
            receiver.accept_offer(manifest.clone()).unwrap().0,
            vec![0, 1]
        );
        receiver
            .accept_chunk(&read_chunk(&source, &manifest, 0).unwrap())
            .unwrap();

        let mut resumed = Receiver::new(root.join("received"));
        assert_eq!(resumed.accept_offer(manifest.clone()).unwrap().0, vec![1]);
        let completed = resumed
            .accept_chunk(&read_chunk(&source, &manifest, 1).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(fs::read(completed).unwrap(), fs::read(source).unwrap());
        let mut duplicate = Receiver::new(root.join("received"));
        let (missing, completed) = duplicate.accept_offer(manifest).unwrap();
        assert!(missing.is_empty() && completed.is_some());
        fs::remove_dir_all(root).ok();
    }
}
