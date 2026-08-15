use chacha20poly1305::{
    aead::{rand_core::RngCore, Aead, OsRng},
    KeyInit, XChaCha20Poly1305, XNonce,
};
use msnnext_protocol::GroupBan;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{error::Error, fs, path::Path};

pub struct History {
    connection: Connection,
    cipher: XChaCha20Poly1305,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Entry {
    pub event_id: String,
    pub peer: String,
    pub direction: String,
    pub kind: String,
    pub body: String,
    pub timestamp_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ContactEntry {
    pub peer: String,
    pub name: String,
    pub link: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeviceEntry {
    pub peer: String,
    pub name: String,
    pub addresses: Vec<String>,
    pub last_seen_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum SyncOperation {
    RecordEvent(Entry),
    UpsertContact {
        contact: ContactEntry,
        changed_at_ms: u64,
    },
    DeleteContact {
        peer: String,
        changed_at_ms: u64,
    },
    ClearConversation {
        peer: String,
        changed_at_ms: u64,
    },
    UpsertGroup {
        group: GroupChatEntry,
        changed_at_ms: u64,
    },
    DeleteGroup {
        id: String,
        changed_at_ms: u64,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SyncRecord {
    pub seq: u64,
    pub op_id: String,
    pub operation: SyncOperation,
}

#[derive(Default)]
pub struct SyncApplyResult {
    pub applied: usize,
    pub contacts_changed: bool,
    pub conversations_changed: bool,
    pub groups_changed: bool,
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
            event_id TEXT,
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
        );
        CREATE TABLE IF NOT EXISTS linked_devices (
            peer TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            addresses BLOB NOT NULL,
            last_seen_ms INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS sync_log (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            op_id TEXT NOT NULL UNIQUE,
            payload BLOB NOT NULL
        );
        CREATE TABLE IF NOT EXISTS sync_versions (
            entity TEXT PRIMARY KEY,
            changed_at_ms INTEGER NOT NULL,
            op_id TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS sync_cursors (
            peer TEXT PRIMARY KEY,
            remote_seq INTEGER NOT NULL DEFAULT 0,
            sent_seq INTEGER NOT NULL DEFAULT 0
        );",
        )?;
        if !column_exists(&connection, "events", "event_id")? {
            connection.execute("ALTER TABLE events ADD COLUMN event_id TEXT", [])?;
        }
        connection.execute(
            "UPDATE events SET event_id = lower(hex(randomblob(16))) WHERE event_id IS NULL OR event_id = ''",
            [],
        )?;
        connection.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS events_event_id ON events(event_id)",
            [],
        )?;
        let history = Self {
            connection,
            cipher: XChaCha20Poly1305::new((&key).into()),
        };
        history.seed_sync_log()?;
        Ok(history)
    }

    pub fn record(
        &self,
        peer: &str,
        direction: &str,
        kind: &str,
        body: &str,
        timestamp_ms: u64,
    ) -> Result<(), Box<dyn Error>> {
        let event = Entry {
            event_id: random_id(),
            peer: peer.to_owned(),
            direction: direction.to_owned(),
            kind: kind.to_owned(),
            body: body.to_owned(),
            timestamp_ms,
        };
        self.commit_local(SyncOperation::RecordEvent(event))?;
        Ok(())
    }

    pub fn latest(&self, limit: usize) -> Result<Vec<Entry>, Box<dyn Error>> {
        let mut statement = self.connection.prepare(
            "SELECT event_id, peer, direction, kind, body, timestamp_ms FROM events ORDER BY id DESC LIMIT ?1"
        )?;
        let rows = statement.query_map([limit as u64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, u64>(5)?,
            ))
        })?;
        let mut entries = Vec::new();
        for row in rows {
            let (event_id, peer, direction, kind, encrypted, timestamp_ms) = row?;
            if encrypted.len() < 24 {
                return Err("riga cronologia danneggiata".into());
            }
            let body = self
                .cipher
                .decrypt(XNonce::from_slice(&encrypted[..24]), &encrypted[24..])
                .map_err(|_| "cronologia non decifrabile")?;
            entries.push(Entry {
                event_id,
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
            "SELECT event_id, peer, direction, kind, body, timestamp_ms
             FROM events WHERE peer = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![peer, limit as u64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, u64>(5)?,
            ))
        })?;
        let mut entries = Vec::new();
        for row in rows {
            let (event_id, peer, direction, kind, encrypted, timestamp_ms) = row?;
            if encrypted.len() < 24 {
                return Err("riga cronologia danneggiata".into());
            }
            let body = self
                .cipher
                .decrypt(XNonce::from_slice(&encrypted[..24]), &encrypted[24..])
                .map_err(|_| "cronologia non decifrabile")?;
            entries.push(Entry {
                event_id,
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
        self.commit_local(SyncOperation::UpsertContact {
            contact: ContactEntry {
                peer: peer.to_owned(),
                name: name.to_owned(),
                link: link.to_owned(),
            },
            changed_at_ms: added_at_ms,
        })?;
        Ok(())
    }

    pub fn ensure_contact(
        &self,
        peer: &str,
        name: &str,
        added_at_ms: u64,
    ) -> Result<(), Box<dyn Error>> {
        let exists = self
            .connection
            .query_row("SELECT 1 FROM contacts WHERE peer = ?1", [peer], |_| Ok(()))
            .optional()?
            .is_some();
        if !exists {
            self.save_contact(peer, name, "", added_at_ms)?;
        }
        Ok(())
    }

    pub fn rename_contact(&self, peer: &str, name: &str) -> Result<(), Box<dyn Error>> {
        let name = name.trim();
        if name.is_empty() || name.len() > 64 {
            return Err("nome contatto non valido".into());
        }
        let contact = self
            .contacts()?
            .into_iter()
            .find(|contact| contact.peer == peer)
            .ok_or("contatto non trovato")?;
        self.save_contact(peer, name, &contact.link, now_ms())?;
        Ok(())
    }

    pub fn clear_conversation(&self, peer: &str) -> Result<(), Box<dyn Error>> {
        self.commit_local(SyncOperation::ClearConversation {
            peer: peer.to_owned(),
            changed_at_ms: now_ms(),
        })?;
        Ok(())
    }

    pub fn delete_contact(&self, peer: &str) -> Result<(), Box<dyn Error>> {
        self.commit_local(SyncOperation::DeleteContact {
            peer: peer.to_owned(),
            changed_at_ms: now_ms(),
        })?;
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
        self.commit_local(SyncOperation::UpsertGroup {
            group: group.clone(),
            changed_at_ms: now_ms(),
        })?;
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
        self.commit_local(SyncOperation::DeleteGroup {
            id: id.to_owned(),
            changed_at_ms: now_ms(),
        })?;
        Ok(())
    }

    pub fn save_device(&self, device: &DeviceEntry) -> Result<(), Box<dyn Error>> {
        let addresses = cbor4ii::serde::to_vec(Vec::new(), &device.addresses)?;
        self.connection.execute(
            "INSERT INTO linked_devices (peer, name, addresses, last_seen_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(peer) DO UPDATE SET
                name = excluded.name,
                addresses = excluded.addresses,
                last_seen_ms = excluded.last_seen_ms",
            params![
                device.peer,
                device.name,
                self.seal(&addresses)?,
                device.last_seen_ms
            ],
        )?;
        Ok(())
    }

    pub fn devices(&self) -> Result<Vec<DeviceEntry>, Box<dyn Error>> {
        let mut statement = self.connection.prepare(
            "SELECT peer, name, addresses, last_seen_ms FROM linked_devices ORDER BY name COLLATE NOCASE",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, u64>(3)?,
            ))
        })?;
        let mut devices = Vec::new();
        for row in rows {
            let (peer, name, addresses, last_seen_ms) = row?;
            devices.push(DeviceEntry {
                peer,
                name,
                addresses: cbor4ii::serde::from_slice(&self.unseal(&addresses)?)?,
                last_seen_ms,
            });
        }
        Ok(devices)
    }

    pub fn sync_cursors(&self, peer: &str) -> Result<(u64, u64), Box<dyn Error>> {
        Ok(self
            .connection
            .query_row(
                "SELECT remote_seq, sent_seq FROM sync_cursors WHERE peer = ?1",
                [peer],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .unwrap_or_default())
    }

    pub fn update_sync_cursors(
        &self,
        peer: &str,
        remote_seq: u64,
        sent_seq: u64,
    ) -> Result<(), Box<dyn Error>> {
        self.connection.execute(
            "INSERT INTO sync_cursors (peer, remote_seq, sent_seq) VALUES (?1, ?2, ?3)
             ON CONFLICT(peer) DO UPDATE SET
                remote_seq = max(sync_cursors.remote_seq, excluded.remote_seq),
                sent_seq = max(sync_cursors.sent_seq, excluded.sent_seq)",
            params![peer, remote_seq, sent_seq],
        )?;
        Ok(())
    }

    pub fn latest_sync_seq(&self) -> Result<u64, Box<dyn Error>> {
        Ok(self
            .connection
            .query_row("SELECT coalesce(max(seq), 0) FROM sync_log", [], |row| {
                row.get(0)
            })?)
    }

    pub fn sync_records_since(
        &self,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<SyncRecord>, Box<dyn Error>> {
        let mut statement = self.connection.prepare(
            "SELECT seq, op_id, payload FROM sync_log WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = statement.query_map(params![after_seq, limit as u64], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        let mut records = Vec::new();
        for row in rows {
            let (seq, op_id, payload) = row?;
            records.push(SyncRecord {
                seq,
                op_id,
                operation: cbor4ii::serde::from_slice(&self.unseal(&payload)?)?,
            });
        }
        Ok(records)
    }

    pub fn apply_sync_records(
        &self,
        records: &[SyncRecord],
    ) -> Result<SyncApplyResult, Box<dyn Error>> {
        let mut result = SyncApplyResult::default();
        for record in records {
            let exists = self
                .connection
                .query_row(
                    "SELECT 1 FROM sync_log WHERE op_id = ?1",
                    [&record.op_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if exists {
                continue;
            }
            if self.apply_operation(&record.op_id, &record.operation)? {
                result.applied += 1;
                match record.operation {
                    SyncOperation::RecordEvent(_) | SyncOperation::ClearConversation { .. } => {
                        result.conversations_changed = true;
                    }
                    SyncOperation::UpsertContact { .. } | SyncOperation::DeleteContact { .. } => {
                        result.contacts_changed = true;
                        result.conversations_changed = true;
                    }
                    SyncOperation::UpsertGroup { .. } | SyncOperation::DeleteGroup { .. } => {
                        result.groups_changed = true;
                        result.conversations_changed = true;
                    }
                }
            }
            self.append_sync_record(&record.op_id, &record.operation)?;
        }
        Ok(result)
    }

    fn commit_local(&self, operation: SyncOperation) -> Result<(), Box<dyn Error>> {
        let op_id = random_id();
        self.apply_operation(&op_id, &operation)?;
        self.append_sync_record(&op_id, &operation)?;
        Ok(())
    }

    fn append_sync_record(
        &self,
        op_id: &str,
        operation: &SyncOperation,
    ) -> Result<(), Box<dyn Error>> {
        let payload = cbor4ii::serde::to_vec(Vec::new(), operation)?;
        self.connection.execute(
            "INSERT OR IGNORE INTO sync_log (op_id, payload) VALUES (?1, ?2)",
            params![op_id, self.seal(&payload)?],
        )?;
        Ok(())
    }

    fn apply_operation(
        &self,
        op_id: &str,
        operation: &SyncOperation,
    ) -> Result<bool, Box<dyn Error>> {
        let applied = match operation {
            SyncOperation::RecordEvent(event) => {
                let cleared_at = self.entity_version(&format!("conversation:{}", event.peer))?;
                if cleared_at.is_some_and(|(timestamp, _)| timestamp >= event.timestamp_ms) {
                    return Ok(false);
                }
                self.connection.execute(
                    "INSERT OR IGNORE INTO events (event_id, peer, direction, kind, body, timestamp_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        event.event_id,
                        event.peer,
                        event.direction,
                        event.kind,
                        self.seal(event.body.as_bytes())?,
                        event.timestamp_ms,
                    ],
                )? > 0
            }
            SyncOperation::UpsertContact {
                contact,
                changed_at_ms,
            } => {
                let entity = format!("contact:{}", contact.peer);
                if !self.accept_version(&entity, *changed_at_ms, op_id)? {
                    return Ok(false);
                }
                self.connection.execute(
                    "INSERT INTO contacts (peer, name, link, added_at_ms) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(peer) DO UPDATE SET
                        name = excluded.name,
                        link = CASE WHEN excluded.link = '' THEN contacts.link ELSE excluded.link END,
                        added_at_ms = excluded.added_at_ms",
                    params![contact.peer, contact.name, contact.link, changed_at_ms],
                )?;
                self.connection.execute(
                    "DELETE FROM ignored_contacts WHERE peer = ?1",
                    [&contact.peer],
                )?;
                true
            }
            SyncOperation::DeleteContact {
                peer,
                changed_at_ms,
            } => {
                let entity = format!("contact:{peer}");
                if !self.accept_version(&entity, *changed_at_ms, op_id)? {
                    return Ok(false);
                }
                self.connection
                    .execute("DELETE FROM events WHERE peer = ?1", [peer])?;
                self.connection
                    .execute("DELETE FROM contacts WHERE peer = ?1", [peer])?;
                self.connection.execute(
                    "INSERT OR IGNORE INTO ignored_contacts (peer) VALUES (?1)",
                    [peer],
                )?;
                self.set_version(&format!("conversation:{peer}"), *changed_at_ms, op_id)?;
                true
            }
            SyncOperation::ClearConversation {
                peer,
                changed_at_ms,
            } => {
                let entity = format!("conversation:{peer}");
                if !self.accept_version(&entity, *changed_at_ms, op_id)? {
                    return Ok(false);
                }
                self.connection.execute(
                    "DELETE FROM events WHERE peer = ?1 AND timestamp_ms <= ?2",
                    params![peer, changed_at_ms],
                )?;
                true
            }
            SyncOperation::UpsertGroup {
                group,
                changed_at_ms,
            } => {
                let entity = format!("group:{}", group.id);
                if !self.accept_version(&entity, *changed_at_ms, op_id)? {
                    return Ok(false);
                }
                let encoded = cbor4ii::serde::to_vec(Vec::new(), group)?;
                self.connection.execute(
                    "INSERT INTO group_chats (id, definition) VALUES (?1, ?2)
                     ON CONFLICT(id) DO UPDATE SET definition = excluded.definition",
                    params![group.id, self.seal(&encoded)?],
                )?;
                true
            }
            SyncOperation::DeleteGroup { id, changed_at_ms } => {
                let entity = format!("group:{id}");
                if !self.accept_version(&entity, *changed_at_ms, op_id)? {
                    return Ok(false);
                }
                let conversation = format!("group:{id}");
                self.connection
                    .execute("DELETE FROM events WHERE peer = ?1", [&conversation])?;
                self.connection
                    .execute("DELETE FROM group_chats WHERE id = ?1", [id])?;
                self.set_version(
                    &format!("conversation:{conversation}"),
                    *changed_at_ms,
                    op_id,
                )?;
                true
            }
        };
        Ok(applied)
    }

    fn accept_version(
        &self,
        entity: &str,
        changed_at_ms: u64,
        op_id: &str,
    ) -> Result<bool, Box<dyn Error>> {
        if self.entity_version(entity)?.is_some_and(|current| {
            current.0 > changed_at_ms || (current.0 == changed_at_ms && current.1.as_str() >= op_id)
        }) {
            return Ok(false);
        }
        self.set_version(entity, changed_at_ms, op_id)?;
        Ok(true)
    }

    fn entity_version(&self, entity: &str) -> Result<Option<(u64, String)>, Box<dyn Error>> {
        Ok(self
            .connection
            .query_row(
                "SELECT changed_at_ms, op_id FROM sync_versions WHERE entity = ?1",
                [entity],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    fn set_version(
        &self,
        entity: &str,
        changed_at_ms: u64,
        op_id: &str,
    ) -> Result<(), Box<dyn Error>> {
        self.connection.execute(
            "INSERT INTO sync_versions (entity, changed_at_ms, op_id) VALUES (?1, ?2, ?3)
             ON CONFLICT(entity) DO UPDATE SET changed_at_ms = excluded.changed_at_ms, op_id = excluded.op_id",
            params![entity, changed_at_ms, op_id],
        )?;
        Ok(())
    }

    fn seed_sync_log(&self) -> Result<(), Box<dyn Error>> {
        if self.latest_sync_seq()? != 0 {
            return Ok(());
        }
        for contact in self.contacts()? {
            self.append_sync_record(
                &random_id(),
                &SyncOperation::UpsertContact {
                    contact,
                    changed_at_ms: 0,
                },
            )?;
        }
        for entry in self.latest(i64::MAX as usize)? {
            self.append_sync_record(&random_id(), &SyncOperation::RecordEvent(entry))?;
        }
        for group in self.group_chats()? {
            self.append_sync_record(
                &random_id(),
                &SyncOperation::UpsertGroup {
                    group,
                    changed_at_ms: 0,
                },
            )?;
        }
        for peer in self.ignored_contacts()? {
            self.append_sync_record(
                &random_id(),
                &SyncOperation::DeleteContact {
                    peer,
                    changed_at_ms: 0,
                },
            )?;
        }
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

fn column_exists(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<bool, Box<dyn Error>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    Ok(rows
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|name| name == column))
}

fn random_id() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

pub fn rekey_database(
    path: &Path,
    old_key: [u8; 32],
    new_key: [u8; 32],
) -> Result<(), Box<dyn Error>> {
    if old_key == new_key || !path.exists() {
        return Ok(());
    }
    let mut connection = Connection::open(path)?;
    let transaction = connection.transaction()?;
    let old_cipher = XChaCha20Poly1305::new((&old_key).into());
    let new_cipher = XChaCha20Poly1305::new((&new_key).into());
    for (table, column) in [
        ("events", "body"),
        ("group_chats", "definition"),
        ("linked_devices", "addresses"),
        ("sync_log", "payload"),
    ] {
        let exists = transaction
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            continue;
        }
        let mut statement = transaction.prepare(&format!("SELECT rowid, {column} FROM {table}"))?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        let encrypted = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for (rowid, value) in encrypted {
            if value.len() < 24 {
                return Err(format!("dato cifrato non valido in {table}").into());
            }
            let plaintext = old_cipher
                .decrypt(XNonce::from_slice(&value[..24]), &value[24..])
                .map_err(|_| format!("dato non decifrabile in {table}"))?;
            let mut nonce = [0u8; 24];
            OsRng.fill_bytes(&mut nonce);
            let ciphertext = new_cipher
                .encrypt(XNonce::from_slice(&nonce), plaintext.as_ref())
                .map_err(|_| format!("ricifratura fallita in {table}"))?;
            let mut replacement = nonce.to_vec();
            replacement.extend(ciphertext);
            transaction.execute(
                &format!("UPDATE {table} SET {column} = ?1 WHERE rowid = ?2"),
                params![replacement, rowid],
            )?;
        }
    }
    transaction.commit()?;
    Ok(())
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

    #[test]
    fn sync_log_merges_records_once_and_propagates_deletions() {
        let source_path =
            std::env::temp_dir().join(format!("msnnext-sync-source-{}.db", std::process::id()));
        let target_path =
            std::env::temp_dir().join(format!("msnnext-sync-target-{}.db", std::process::id()));
        fs::remove_file(&source_path).ok();
        fs::remove_file(&target_path).ok();
        let source = History::open(&source_path, [21; 32]).unwrap();
        let target = History::open(&target_path, [22; 32]).unwrap();
        source
            .save_contact("peer-a", "Alice", "msnnext://add/alice", 10)
            .unwrap();
        source.record("peer-a", "in", "text", "ciao", 11).unwrap();

        let first = source.sync_records_since(0, 100).unwrap();
        assert_eq!(target.apply_sync_records(&first).unwrap().applied, 2);
        assert_eq!(target.apply_sync_records(&first).unwrap().applied, 0);
        assert_eq!(target.contacts().unwrap().len(), 1);
        assert_eq!(target.conversation("peer-a", 10).unwrap().len(), 1);

        let cursor = first.last().unwrap().seq;
        source.delete_contact("peer-a").unwrap();
        let deletion = source.sync_records_since(cursor, 100).unwrap();
        assert_eq!(target.apply_sync_records(&deletion).unwrap().applied, 1);
        assert!(target.contacts().unwrap().is_empty());
        assert!(target.conversation("peer-a", 10).unwrap().is_empty());

        drop(source);
        drop(target);
        fs::remove_file(source_path).ok();
        fs::remove_file(target_path).ok();
    }

    #[test]
    fn history_can_be_rekeyed_for_a_new_device_identity() {
        let path = std::env::temp_dir().join(format!("msnnext-rekey-{}.db", std::process::id()));
        fs::remove_file(&path).ok();
        {
            let history = History::open(&path, [31; 32]).unwrap();
            history
                .record("peer", "in", "text", "trasferito", 1)
                .unwrap();
        }
        rekey_database(&path, [31; 32], [32; 32]).unwrap();
        let history = History::open(&path, [32; 32]).unwrap();
        assert_eq!(history.latest(1).unwrap()[0].body, "trasferito");
        drop(history);
        fs::remove_file(path).ok();
    }
}
