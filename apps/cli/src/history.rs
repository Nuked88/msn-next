use chacha20poly1305::{
    aead::{rand_core::RngCore, Aead, OsRng},
    KeyInit, XChaCha20Poly1305, XNonce,
};
use rusqlite::{params, Connection};
use std::{error::Error, fs, path::Path};

pub struct History {
    connection: Connection,
    cipher: XChaCha20Poly1305,
}

pub struct Entry {
    pub peer: String,
    pub direction: String,
    pub kind: String,
    pub body: String,
    pub timestamp_ms: u64,
}

impl History {
    pub fn open(path: &Path, key: [u8; 32]) -> Result<Self, Box<dyn Error>> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY,
            peer TEXT NOT NULL,
            direction TEXT NOT NULL,
            kind TEXT NOT NULL,
            body BLOB NOT NULL,
            timestamp_ms INTEGER NOT NULL
        );",
        )?;
        Ok(Self {
            connection,
            cipher: XChaCha20Poly1305::new((&key).into()),
        })
    }

    pub fn record(
        &self,
        peer: &str,
        direction: &str,
        kind: &str,
        body: &str,
        timestamp_ms: u64,
    ) -> Result<(), Box<dyn Error>> {
        let mut nonce = [0; 24];
        OsRng.fill_bytes(&mut nonce);
        let ciphertext = self
            .cipher
            .encrypt(XNonce::from_slice(&nonce), body.as_bytes())
            .map_err(|_| "cifratura cronologia fallita")?;
        let mut encrypted = nonce.to_vec();
        encrypted.extend(ciphertext);
        self.connection.execute(
            "INSERT INTO events (peer, direction, kind, body, timestamp_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![peer, direction, kind, encrypted, timestamp_ms],
        )?;
        Ok(())
    }

    pub fn latest(&self, limit: usize) -> Result<Vec<Entry>, Box<dyn Error>> {
        let mut statement = self.connection.prepare(
            "SELECT peer, direction, kind, body, timestamp_ms FROM events ORDER BY id DESC LIMIT ?1"
        )?;
        let rows = statement.query_map([limit as u64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, u64>(4)?,
            ))
        })?;
        let mut entries = Vec::new();
        for row in rows {
            let (peer, direction, kind, encrypted, timestamp_ms) = row?;
            if encrypted.len() < 24 {
                return Err("riga cronologia danneggiata".into());
            }
            let body = self
                .cipher
                .decrypt(XNonce::from_slice(&encrypted[..24]), &encrypted[24..])
                .map_err(|_| "cronologia non decifrabile")?;
            entries.push(Entry {
                peer,
                direction,
                kind,
                body: String::from_utf8(body)?,
                timestamp_ms,
            });
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_round_trips_without_plaintext_on_disk() {
        let path = std::env::temp_dir().join(format!("msnnext-history-{}.db", std::process::id()));
        fs::remove_file(&path).ok();
        {
            let history = History::open(&path, [7; 32]).unwrap();
            history
                .record("peer", "in", "text", "segreto-inconfondibile", 42)
                .unwrap();
            let entries = history.latest(1).unwrap();
            assert_eq!(entries[0].body, "segreto-inconfondibile");
        }
        assert!(!fs::read(&path)
            .unwrap()
            .windows(22)
            .any(|bytes| bytes == b"segreto-inconfondibile"));
        fs::remove_file(path).ok();
    }
}
