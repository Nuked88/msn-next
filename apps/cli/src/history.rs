use chacha20poly1305::{
    aead::{rand_core::RngCore, Aead, OsRng},
    KeyInit, XChaCha20Poly1305, XNonce,
};
use msnnext_protocol::GroupBan;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
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

pub struct ContactEntry {
    pub peer: String,
    pub name: String,
    pub link: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GroupChatEntry {
    pub id: String,
    pub name: String,
    pub owner_peer: String,
    pub members: Vec<String>,
    #[serde(default)]
    pub admins: Vec<String>,
    #[serde(default)]
    pub silenced: Vec<String>,
    #[serde(default)]
    pub bans: Vec<GroupBan>,
    pub revision: u64,
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
        );
        CREATE TABLE IF NOT EXISTS contacts (
            peer TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            link TEXT NOT NULL,
            added_at_ms INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS ignored_contacts (
            peer TEXT PRIMARY KEY
        );
        CREATE TABLE IF NOT EXISTS group_chats (
            id TEXT PRIMARY KEY,
            definition BLOB NOT NULL
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

    pub fn conversation(&self, peer: &str, limit: usize) -> Result<Vec<Entry>, Box<dyn Error>> {
        let mut statement = self.connection.prepare(
            "SELECT peer, direction, kind, body, timestamp_ms
             FROM events WHERE peer = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![peer, limit as u64], |row| {
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

    pub fn save_contact(
        &self,
        peer: &str,
        name: &str,
        link: &str,
        added_at_ms: u64,
    ) -> Result<(), Box<dyn Error>> {
        self.connection.execute(
            "INSERT INTO contacts (peer, name, link, added_at_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(peer) DO UPDATE SET
                name = excluded.name,
                link = CASE WHEN excluded.link = '' THEN contacts.link ELSE excluded.link END",
            params![peer, name, link, added_at_ms],
        )?;
        Ok(())
    }

    pub fn ensure_contact(
        &self,
        peer: &str,
        name: &str,
        added_at_ms: u64,
    ) -> Result<(), Box<dyn Error>> {
        self.connection.execute(
            "INSERT OR IGNORE INTO contacts (peer, name, link, added_at_ms) VALUES (?1, ?2, '', ?3)",
            params![peer, name, added_at_ms],
        )?;
        Ok(())
    }

    pub fn rename_contact(&self, peer: &str, name: &str) -> Result<(), Box<dyn Error>> {
        let name = name.trim();
        if name.is_empty() || name.len() > 64 {
            return Err("nome contatto non valido".into());
        }
        if self.connection.execute(
            "UPDATE contacts SET name = ?2 WHERE peer = ?1",
            params![peer, name],
        )? == 0
        {
            return Err("contatto non trovato".into());
        }
        Ok(())
    }

    pub fn clear_conversation(&self, peer: &str) -> Result<(), Box<dyn Error>> {
        self.connection
            .execute("DELETE FROM events WHERE peer = ?1", [peer])?;
        Ok(())
    }

    pub fn delete_contact(&self, peer: &str) -> Result<(), Box<dyn Error>> {
        self.clear_conversation(peer)?;
        self.connection
            .execute("DELETE FROM contacts WHERE peer = ?1", [peer])?;
        self.connection.execute(
            "INSERT OR IGNORE INTO ignored_contacts (peer) VALUES (?1)",
            [peer],
        )?;
        Ok(())
    }

    pub fn allow_contact(&self, peer: &str) -> Result<(), Box<dyn Error>> {
        self.connection
            .execute("DELETE FROM ignored_contacts WHERE peer = ?1", [peer])?;
        Ok(())
    }

    pub fn ignored_contacts(&self) -> Result<Vec<String>, Box<dyn Error>> {
        let mut statement = self
            .connection
            .prepare("SELECT peer FROM ignored_contacts")?;
        let rows = statement.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn contacts(&self) -> Result<Vec<ContactEntry>, Box<dyn Error>> {
        let mut statement = self
            .connection
            .prepare("SELECT peer, name, link FROM contacts ORDER BY name COLLATE NOCASE")?;
        let rows = statement.query_map([], |row| {
            Ok(ContactEntry {
                peer: row.get(0)?,
                name: row.get(1)?,
                link: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn save_group_chat(&self, group: &GroupChatEntry) -> Result<(), Box<dyn Error>> {
        if group.id.len() != 32 || !group.id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("id chat di gruppo non valido".into());
        }
        let encoded = cbor4ii::serde::to_vec(Vec::new(), group)?;
        let encrypted = self.seal(&encoded)?;
        self.connection.execute(
            "INSERT INTO group_chats (id, definition) VALUES (?1, ?2)
             ON CONFLICT(id) DO UPDATE SET definition = excluded.definition",
            params![group.id, encrypted],
        )?;
        Ok(())
    }

    pub fn group_chat(&self, id: &str) -> Result<Option<GroupChatEntry>, Box<dyn Error>> {
        let mut statement = self
            .connection
            .prepare("SELECT definition FROM group_chats WHERE id = ?1")?;
        let mut rows = statement.query([id])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let encrypted: Vec<u8> = row.get(0)?;
        Ok(Some(cbor4ii::serde::from_slice(&self.unseal(&encrypted)?)?))
    }

    pub fn group_chats(&self) -> Result<Vec<GroupChatEntry>, Box<dyn Error>> {
        let mut statement = self
            .connection
            .prepare("SELECT definition FROM group_chats ORDER BY id")?;
        let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        let mut groups = Vec::new();
        for row in rows {
            groups.push(cbor4ii::serde::from_slice(&self.unseal(&row?)?)?);
        }
        groups.sort_by(|left: &GroupChatEntry, right: &GroupChatEntry| {
            left.name.to_lowercase().cmp(&right.name.to_lowercase())
        });
        Ok(groups)
    }

    pub fn delete_group_chat(&self, id: &str) -> Result<(), Box<dyn Error>> {
        self.clear_conversation(&format!("group:{id}"))?;
        self.connection
            .execute("DELETE FROM group_chats WHERE id = ?1", [id])?;
        Ok(())
    }

    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
        let mut nonce = [0; 24];
        OsRng.fill_bytes(&mut nonce);
        let ciphertext = self
            .cipher
            .encrypt(XNonce::from_slice(&nonce), plaintext)
            .map_err(|_| "cifratura dati fallita")?;
        let mut encrypted = nonce.to_vec();
        encrypted.extend(ciphertext);
        Ok(encrypted)
    }

    fn unseal(&self, encrypted: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
        if encrypted.len() < 24 {
            return Err("dato cifrato danneggiato".into());
        }
        self.cipher
            .decrypt(XNonce::from_slice(&encrypted[..24]), &encrypted[24..])
            .map_err(|_| "dato non decifrabile".into())
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

    #[test]
    fn conversation_history_is_filtered_by_peer() {
        let path =
            std::env::temp_dir().join(format!("msnnext-conversation-{}.db", std::process::id()));
        fs::remove_file(&path).ok();
        let history = History::open(&path, [8; 32]).unwrap();
        history.record("alice", "in", "text", "uno", 1).unwrap();
        history.record("bob", "in", "text", "due", 2).unwrap();

        let entries = history.conversation("alice", 20).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].peer, "alice");
        assert_eq!(entries[0].body, "uno");
        drop(history);
        fs::remove_file(path).ok();
    }

    #[test]
    fn contacts_survive_reopening_the_store() {
        let path = std::env::temp_dir().join(format!("msnnext-contacts-{}.db", std::process::id()));
        fs::remove_file(&path).ok();
        {
            let history = History::open(&path, [9; 32]).unwrap();
            history
                .save_contact("peer-a", "Alice", "msnnext://add/alice", 7)
                .unwrap();
        }

        let history = History::open(&path, [9; 32]).unwrap();
        let contacts = history.contacts().unwrap();

        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].peer, "peer-a");
        assert_eq!(contacts[0].name, "Alice");
        assert_eq!(contacts[0].link, "msnnext://add/alice");
        drop(history);
        fs::remove_file(path).ok();
    }

    #[test]
    fn contact_can_be_renamed_and_removed_with_its_chat() {
        let path =
            std::env::temp_dir().join(format!("msnnext-contact-crud-{}.db", std::process::id()));
        fs::remove_file(&path).ok();
        let history = History::open(&path, [10; 32]).unwrap();
        history.save_contact("peer-a", "Alice", "link", 1).unwrap();
        history.record("peer-a", "in", "text", "ciao", 2).unwrap();
        history.rename_contact("peer-a", "Alicia").unwrap();
        assert_eq!(history.contacts().unwrap()[0].name, "Alicia");
        history.delete_contact("peer-a").unwrap();
        assert!(history.contacts().unwrap().is_empty());
        assert!(history.conversation("peer-a", 10).unwrap().is_empty());
        assert_eq!(history.ignored_contacts().unwrap(), vec!["peer-a"]);
        history.allow_contact("peer-a").unwrap();
        assert!(history.ignored_contacts().unwrap().is_empty());
        drop(history);
        fs::remove_file(path).ok();
    }

    #[test]
    fn group_chat_definition_is_persistent_and_encrypted() {
        let path =
            std::env::temp_dir().join(format!("msnnext-group-chat-{}.db", std::process::id()));
        fs::remove_file(&path).ok();
        let group = GroupChatEntry {
            id: "00112233445566778899aabbccddeeff".into(),
            name: "Segreto gruppo".into(),
            owner_peer: "owner".into(),
            members: vec!["owner".into(), "alice".into(), "bob".into()],
            admins: Vec::new(),
            silenced: Vec::new(),
            bans: Vec::new(),
            revision: 1,
        };
        {
            let history = History::open(&path, [11; 32]).unwrap();
            history.save_group_chat(&group).unwrap();
            assert_eq!(
                history.group_chat(&group.id).unwrap().unwrap().name,
                group.name
            );
        }
        assert!(!fs::read(&path)
            .unwrap()
            .windows(group.name.len())
            .any(|bytes| bytes == group.name.as_bytes()));
        fs::remove_file(path).ok();
    }
}
