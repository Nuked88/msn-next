use chacha20poly1305::{
    aead::{rand_core::RngCore, Aead, OsRng},
    KeyInit, XChaCha20Poly1305, XNonce,
};
use msnnext_protocol::GroupBan;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{error::Error, fs, path::Path};

/// `(peer, direction, timestamp_ms)` di un evento in cronologia.
pub type EventMeta = (String, String, u64);

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
    /// Account a cui appartiene il contatto: identifica la persona, non il
    /// singolo dispositivo. Vuoto finché il contatto non presenta la prova.
    /// `default` per compatibilità con i client più vecchi.
    #[serde(default)]
    pub account_id: String,
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
    /// Cancella un singolo messaggio solo su questo account/dispositivi.
    DeleteEvent {
        event_id: String,
        changed_at_ms: u64,
    },
    /// Segna un messaggio come eliminato "per tutti" (mostra un segnaposto).
    TombstoneEvent {
        event_id: String,
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

/// Byte di record per richiesta Pair/Sync. Il codec cbor di libp2p rifiuta
/// richieste oltre 1 MiB, e il messaggio sigillato codifica il ciphertext
/// come array (fino a 2 byte per byte): resta ampio margine.
pub const MAX_SYNC_BATCH_BYTES: usize = 256 * 1024;

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
        // Le righe cancellate vengono azzerate su disco, non solo scollegate.
        connection.pragma_update(None, "secure_delete", true)?;
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
            payload BLOB NOT NULL,
            event_id TEXT,
            peer TEXT
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
        );
        CREATE TABLE IF NOT EXISTS deleted_events (
            event_id TEXT PRIMARY KEY,
            tombstone INTEGER NOT NULL,
            changed_at_ms INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS revoked_devices (
            peer TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            revoked_at_ms INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS retired_account_keys (
            key_id BLOB PRIMARY KEY,
            account_key BLOB NOT NULL
        );
        CREATE TABLE IF NOT EXISTS pq_identities (
            peer TEXT PRIMARY KEY,
            ml_dsa_public_key BLOB NOT NULL
        );",
        )?;
        if !column_exists(&connection, "contacts", "account_id")? {
            connection.execute(
                "ALTER TABLE contacts ADD COLUMN account_id TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
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
        // Il sync_log ricorda a quale messaggio e conversazione si riferisce
        // ogni RecordEvent, così una cancellazione può toglierne il contenuto.
        let index_sync_log = !column_exists(&connection, "sync_log", "event_id")?;
        if index_sync_log {
            connection.execute("ALTER TABLE sync_log ADD COLUMN event_id TEXT", [])?;
            connection.execute("ALTER TABLE sync_log ADD COLUMN peer TEXT", [])?;
        }
        connection.execute_batch(
            "CREATE INDEX IF NOT EXISTS sync_log_event_id ON sync_log(event_id);
             CREATE INDEX IF NOT EXISTS sync_log_peer ON sync_log(peer);",
        )?;
        let history = Self {
            connection,
            cipher: XChaCha20Poly1305::new((&key).into()),
        };
        if index_sync_log {
            history.compact_sync_log()?;
        }
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

    /// Come `record` ma con un `event_id` esplicito e condiviso tra i peer,
    /// necessario per referenziare il messaggio nella cancellazione.
    pub fn record_with_id(
        &self,
        event_id: &str,
        peer: &str,
        direction: &str,
        kind: &str,
        body: &str,
        timestamp_ms: u64,
    ) -> Result<(), Box<dyn Error>> {
        let event = Entry {
            event_id: event_id.to_owned(),
            peer: peer.to_owned(),
            direction: direction.to_owned(),
            kind: kind.to_owned(),
            body: body.to_owned(),
            timestamp_ms,
        };
        self.commit_local(SyncOperation::RecordEvent(event))?;
        Ok(())
    }

    /// Cancella un messaggio solo localmente (e sugli altri dispositivi propri).
    pub fn delete_event(&self, event_id: &str, changed_at_ms: u64) -> Result<(), Box<dyn Error>> {
        self.commit_local(SyncOperation::DeleteEvent {
            event_id: event_id.to_owned(),
            changed_at_ms,
        })?;
        Ok(())
    }

    /// Segna un messaggio come eliminato "per tutti" (segnaposto su entrambi).
    pub fn tombstone_event(
        &self,
        event_id: &str,
        changed_at_ms: u64,
    ) -> Result<(), Box<dyn Error>> {
        self.commit_local(SyncOperation::TombstoneEvent {
            event_id: event_id.to_owned(),
            changed_at_ms,
        })?;
        Ok(())
    }

    /// Metadati di un messaggio per validare una cancellazione in ingresso:
    /// `(peer, direction, timestamp_ms)`.
    pub fn event_meta(&self, event_id: &str) -> Result<Option<EventMeta>, Box<dyn Error>> {
        Ok(self
            .connection
            .query_row(
                "SELECT peer, direction, timestamp_ms FROM events WHERE event_id = ?1",
                [event_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, u64>(2)?,
                    ))
                },
            )
            .optional()?)
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
        self.conversation_before(peer, None, limit)
    }

    /// I messaggi di una conversazione dal più recente, fino a `limit`. Con
    /// `before` (id di un evento) solo quelli più vecchi di quell'evento: è il
    /// cursore della paginazione.
    pub fn conversation_before(
        &self,
        peer: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entry>, Box<dyn Error>> {
        let mut statement = self.connection.prepare(
            "SELECT event_id, peer, direction, kind, body, timestamp_ms
             FROM events
             WHERE peer = ?1
               AND (?2 IS NULL OR id < (SELECT id FROM events WHERE event_id = ?2))
             ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = statement.query_map(params![peer, before, limit as u64], |row| {
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
                account_id: String::new(),
            },
            changed_at_ms: added_at_ms,
        })?;
        Ok(())
    }

    /// Lega il contatto all'account che ha firmato la prova ricevuta nella
    /// presence. Il legame viaggia nel log di sincronizzazione, così anche gli
    /// altri dispositivi propri riconoscono i dispositivi di quel contatto.
    pub fn bind_contact_account(
        &self,
        peer: &str,
        account_id: &str,
        changed_at_ms: u64,
    ) -> Result<(), Box<dyn Error>> {
        // Un peer senza riga in rubrica (per esempio il peer passato a mano con
        // --connect) non ha nulla da legare: non è un errore.
        let Some(contact) = self
            .contacts()?
            .into_iter()
            .find(|contact| contact.peer == peer)
            .filter(|contact| contact.account_id != account_id)
        else {
            return Ok(());
        };
        self.commit_local(SyncOperation::UpsertContact {
            contact: ContactEntry {
                account_id: account_id.to_owned(),
                ..contact
            },
            changed_at_ms,
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
        // Un contatto rimosso e riaggiunto riparte da zero: la sua chiave
        // ML-DSA si fissa di nuovo al prossimo handshake.
        self.connection
            .execute("DELETE FROM pq_identities WHERE peer = ?1", [peer])?;
        Ok(())
    }

    /// Fonde due contatti che si sono rivelati la stessa persona (stesso
    /// account): la cronologia del duplicato passa sotto il contatto superstite
    /// e la voce duplicata sparisce dalla rubrica. Usa solo operazioni di sync
    /// già esistenti, così anche i dispositivi con una build più vecchia
    /// restano sincronizzabili.
    pub fn merge_contact(
        &self,
        from: &str,
        into: &str,
        changed_at_ms: u64,
    ) -> Result<usize, Box<dyn Error>> {
        if from == into {
            return Ok(0);
        }
        let events = self.conversation(from, i64::MAX as usize)?;
        // Prima la cancellazione, poi il travaso: sull'altro dispositivo le
        // operazioni si applicano nello stesso ordine e la `DeleteContact`
        // (che rimuove gli eventi del duplicato) non cancella quelli appena
        // spostati.
        self.commit_local(SyncOperation::DeleteContact {
            peer: from.to_owned(),
            changed_at_ms,
        })?;
        // Il duplicato non è una persona bloccata: è un dispositivo del
        // contatto superstite e deve restare raggiungibile.
        self.allow_contact(from)?;
        let moved = events.len();
        for event in events.into_iter().rev() {
            self.commit_local(SyncOperation::RecordEvent(Entry {
                peer: into.to_owned(),
                ..event
            }))?;
        }
        Ok(moved)
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
        let mut statement = self.connection.prepare(
            "SELECT peer, name, link, account_id FROM contacts ORDER BY name COLLATE NOCASE",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(ContactEntry {
                peer: row.get(0)?,
                name: row.get(1)?,
                link: row.get(2)?,
                account_id: row.get(3)?,
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

    /// Esclude un dispositivo dall'account: niente più sync né relay con lui,
    /// anche dopo un riavvio. Un nuovo pairing lo riammette.
    pub fn revoke_device(&self, peer: &str, name: &str) -> Result<(), Box<dyn Error>> {
        self.connection
            .execute("DELETE FROM linked_devices WHERE peer = ?1", [peer])?;
        self.connection
            .execute("DELETE FROM sync_cursors WHERE peer = ?1", [peer])?;
        self.connection.execute(
            "INSERT OR REPLACE INTO revoked_devices (peer, name, revoked_at_ms) VALUES (?1, ?2, ?3)",
            params![peer, name, now_ms()],
        )?;
        Ok(())
    }

    /// Chiave ML-DSA fissata per un peer al primo handshake firmato.
    pub fn pq_identity(&self, peer: &str) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
        Ok(self
            .connection
            .query_row(
                "SELECT ml_dsa_public_key FROM pq_identities WHERE peer = ?1",
                [peer],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn pin_pq_identity(&self, peer: &str, key: &[u8]) -> Result<(), Box<dyn Error>> {
        self.connection.execute(
            "INSERT OR REPLACE INTO pq_identities (peer, ml_dsa_public_key) VALUES (?1, ?2)",
            params![peer, key],
        )?;
        Ok(())
    }

    pub fn readmit_device(&self, peer: &str) -> Result<(), Box<dyn Error>> {
        self.connection
            .execute("DELETE FROM revoked_devices WHERE peer = ?1", [peer])?;
        Ok(())
    }

    /// Dispositivi revocati: `(peer, nome)`.
    pub fn revoked_devices(&self) -> Result<Vec<(String, String)>, Box<dyn Error>> {
        let mut statement = self
            .connection
            .prepare("SELECT peer, name FROM revoked_devices ORDER BY peer")?;
        let peers = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(peers)
    }

    /// Conserva una chiave di account sostituita: serve a riconoscere i
    /// dispositivi rimasti offline durante la rotazione e passare loro la
    /// chiave nuova.
    pub fn retire_account_key(
        &self,
        key_id: &[u8; 16],
        key: &[u8; 32],
    ) -> Result<(), Box<dyn Error>> {
        self.connection.execute(
            "INSERT OR REPLACE INTO retired_account_keys (key_id, account_key) VALUES (?1, ?2)",
            params![key_id.as_slice(), self.seal(key)?],
        )?;
        Ok(())
    }

    pub fn retired_account_key(
        &self,
        key_id: &[u8; 16],
    ) -> Result<Option<[u8; 32]>, Box<dyn Error>> {
        let sealed: Option<Vec<u8>> = self
            .connection
            .query_row(
                "SELECT account_key FROM retired_account_keys WHERE key_id = ?1",
                [key_id.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        sealed
            .map(|sealed| {
                let key: [u8; 32] = self
                    .unseal(&sealed)?
                    .try_into()
                    .map_err(|_| "chiave di account ritirata non valida")?;
                Ok(key)
            })
            .transpose()
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
            // Dal contatore e non da max(seq): le righe compattate lasciano
            // buchi, anche in fondo al log.
            .query_row(
                "SELECT max(
                    coalesce((SELECT max(seq) FROM sync_log), 0),
                    coalesce((SELECT seq FROM sqlite_sequence WHERE name = 'sync_log'), 0)
                 )",
                [],
                |row| row.get(0),
            )?)
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
        let mut batch_bytes = 0;
        for row in rows {
            let (seq, op_id, payload) = row?;
            let plain = self.unseal(&payload)?;
            // Il resto arriva al giro successivo: chi riceve ripete il sync
            // finché il cursore non raggiunge latest_seq.
            batch_bytes += plain.len();
            if !records.is_empty() && batch_bytes > MAX_SYNC_BATCH_BYTES {
                break;
            }
            records.push(SyncRecord {
                seq,
                op_id,
                operation: cbor4ii::serde::from_slice(&plain)?,
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
                    SyncOperation::RecordEvent(_)
                    | SyncOperation::ClearConversation { .. }
                    | SyncOperation::DeleteEvent { .. }
                    | SyncOperation::TombstoneEvent { .. } => {
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

    fn commit_local(&self, mut operation: SyncOperation) -> Result<(), Box<dyn Error>> {
        let op_id = random_id();
        // Orologio ibrido: una modifica fatta qui viene dopo tutto ciò che
        // questo dispositivo ha già visto per la stessa entità, anche se il
        // suo orologio è indietro rispetto a quello di chi l'ha scritta.
        if let Some((entity, changed_at_ms)) = versioned_entity(&mut operation) {
            if let Some((seen, _)) = self.entity_version(&entity)? {
                *changed_at_ms = (*changed_at_ms).max(seen.saturating_add(1));
            }
        }
        self.apply_operation(&op_id, &operation)?;
        self.append_sync_record(&op_id, &operation)?;
        Ok(())
    }

    fn append_sync_record(
        &self,
        op_id: &str,
        operation: &SyncOperation,
    ) -> Result<(), Box<dyn Error>> {
        // Un messaggio già cancellato non entra nel log con il suo testo,
        // nemmeno se arriva da un altro dispositivo dopo la cancellazione.
        let redacted = self.redacted(operation)?;
        let operation = redacted.as_ref().unwrap_or(operation);
        let (event_id, peer) = sync_subject(operation);
        let payload = cbor4ii::serde::to_vec(Vec::new(), operation)?;
        self.connection.execute(
            "INSERT OR IGNORE INTO sync_log (op_id, payload, event_id, peer) VALUES (?1, ?2, ?3, ?4)",
            params![op_id, self.seal(&payload)?, event_id, peer],
        )?;
        Ok(())
    }

    /// La versione di un'operazione da conservare nel sync_log, se diversa:
    /// un RecordEvent di un messaggio cancellato perde il contenuto. Il log
    /// viene reinviato a ogni nuovo dispositivo collegato, quindi senza
    /// questo i messaggi "cancellati" restavano su disco e si propagavano.
    fn redacted(&self, operation: &SyncOperation) -> Result<Option<SyncOperation>, Box<dyn Error>> {
        let SyncOperation::RecordEvent(event) = operation else {
            return Ok(None);
        };
        let deleted: Option<(i64, u64)> = self
            .connection
            .query_row(
                "SELECT tombstone, changed_at_ms FROM deleted_events WHERE event_id = ?1",
                [&event.event_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let removed_at = match deleted {
            Some((0, changed_at_ms)) => Some(changed_at_ms),
            Some(_) if event.kind == "deleted" && event.body.is_empty() => return Ok(None),
            Some(_) => {
                return Ok(Some(SyncOperation::RecordEvent(Entry {
                    kind: "deleted".into(),
                    body: String::new(),
                    ..event.clone()
                })))
            }
            None => {
                // Conversazione svuotata (o contatto/gruppo eliminato) dopo
                // questo messaggio. Non un DeleteEvent: con una fusione di
                // contatti lo stesso id vive ancora sotto il contatto superstite.
                let cleared_at = self
                    .entity_version(&format!("conversation:{}", event.peer))?
                    .map(|(cleared_at, _)| cleared_at)
                    .filter(|cleared_at| *cleared_at >= event.timestamp_ms);
                return Ok(
                    cleared_at.map(|changed_at_ms| SyncOperation::ClearConversation {
                        peer: event.peer.clone(),
                        changed_at_ms,
                    }),
                );
            }
        };
        Ok(removed_at.map(|changed_at_ms| SyncOperation::DeleteEvent {
            event_id: event.event_id.clone(),
            changed_at_ms,
        }))
    }

    /// Riscrive le righe del sync_log di un messaggio (`event_id`) o di una
    /// conversazione (`peer`) dopo una cancellazione.
    fn redact_sync_log(&self, column: &str, value: &str) -> Result<(), Box<dyn Error>> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT seq, payload FROM sync_log WHERE {column} = ?1"
        ))?;
        let rows = statement
            .query_map([value], |row| {
                Ok((row.get::<_, u64>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (seq, payload) in rows {
            let operation: SyncOperation = cbor4ii::serde::from_slice(&self.unseal(&payload)?)?;
            if let Some(redacted) = self.redacted(&operation)? {
                self.replace_sync_row(seq, &redacted)?;
            }
        }
        Ok(())
    }

    /// Un messaggio eliminato per tutti resta come segnaposto; per il resto la
    /// riga sparisce: l'operazione di cancellazione è già nel log e basta ai
    /// nuovi dispositivi.
    fn replace_sync_row(&self, seq: u64, redacted: &SyncOperation) -> Result<(), Box<dyn Error>> {
        if matches!(redacted, SyncOperation::RecordEvent(_)) {
            self.rewrite_sync_row(seq, redacted)
        } else {
            self.connection
                .execute("DELETE FROM sync_log WHERE seq = ?1", [seq])?;
            Ok(())
        }
    }

    fn rewrite_sync_row(&self, seq: u64, operation: &SyncOperation) -> Result<(), Box<dyn Error>> {
        let (event_id, peer) = sync_subject(operation);
        let payload = cbor4ii::serde::to_vec(Vec::new(), operation)?;
        self.connection.execute(
            "UPDATE sync_log SET payload = ?2, event_id = ?3, peer = ?4 WHERE seq = ?1",
            params![seq, self.seal(&payload)?, event_id, peer],
        )?;
        Ok(())
    }

    /// Una tantum, sui database creati prima dell'indice: annota ogni riga e
    /// toglie il contenuto dei messaggi già cancellati.
    fn compact_sync_log(&self) -> Result<(), Box<dyn Error>> {
        let rows = {
            let mut statement = self
                .connection
                .prepare("SELECT seq, payload FROM sync_log")?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, u64>(0)?, row.get::<_, Vec<u8>>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        for (seq, payload) in rows {
            let operation: SyncOperation = cbor4ii::serde::from_slice(&self.unseal(&payload)?)?;
            match self.redacted(&operation)? {
                Some(redacted) => self.replace_sync_row(seq, &redacted)?,
                None => self.rewrite_sync_row(seq, &operation)?,
            }
        }
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
                // Un messaggio già cancellato non deve "resuscitare" via sync.
                let deleted: Option<i64> = self
                    .connection
                    .query_row(
                        "SELECT tombstone FROM deleted_events WHERE event_id = ?1",
                        [&event.event_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                match deleted {
                    Some(0) => return Ok(false),
                    Some(_) => self.connection.execute(
                        "INSERT OR IGNORE INTO events (event_id, peer, direction, kind, body, timestamp_ms)
                         VALUES (?1, ?2, ?3, 'deleted', ?4, ?5)",
                        params![
                            event.event_id,
                            event.peer,
                            event.direction,
                            self.seal(b"")?,
                            event.timestamp_ms,
                        ],
                    )? > 0,
                    None => self.connection.execute(
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
                    )? > 0,
                }
            }
            SyncOperation::DeleteEvent {
                event_id,
                changed_at_ms,
            } => {
                self.connection
                    .execute("DELETE FROM events WHERE event_id = ?1", [event_id])?;
                self.connection.execute(
                    "INSERT INTO deleted_events (event_id, tombstone, changed_at_ms) VALUES (?1, 0, ?2)
                     ON CONFLICT(event_id) DO UPDATE SET tombstone = 0, changed_at_ms = ?2",
                    params![event_id, changed_at_ms],
                )?;
                self.redact_sync_log("event_id", event_id)?;
                true
            }
            SyncOperation::TombstoneEvent {
                event_id,
                changed_at_ms,
            } => {
                self.connection.execute(
                    "UPDATE events SET kind = 'deleted', body = ?2 WHERE event_id = ?1",
                    params![event_id, self.seal(b"")?],
                )?;
                self.connection.execute(
                    "INSERT INTO deleted_events (event_id, tombstone, changed_at_ms) VALUES (?1, 1, ?2)
                     ON CONFLICT(event_id) DO UPDATE SET tombstone = 1, changed_at_ms = ?2",
                    params![event_id, changed_at_ms],
                )?;
                self.redact_sync_log("event_id", event_id)?;
                true
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
                    "INSERT INTO contacts (peer, name, link, added_at_ms, account_id)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(peer) DO UPDATE SET
                        name = excluded.name,
                        link = CASE WHEN excluded.link = '' THEN contacts.link ELSE excluded.link END,
                        added_at_ms = excluded.added_at_ms,
                        account_id = CASE WHEN excluded.account_id = ''
                            THEN contacts.account_id ELSE excluded.account_id END",
                    params![
                        contact.peer,
                        contact.name,
                        contact.link,
                        changed_at_ms,
                        contact.account_id
                    ],
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
                self.redact_sync_log("peer", peer)?;
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
                self.redact_sync_log("peer", peer)?;
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
                self.redact_sync_log("peer", &conversation)?;
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

/// Messaggio e conversazione a cui si riferisce una riga del sync_log.
fn sync_subject(operation: &SyncOperation) -> (Option<&str>, Option<&str>) {
    match operation {
        SyncOperation::RecordEvent(event) => (Some(&event.event_id), Some(&event.peer)),
        _ => (None, None),
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

/// L'entità di `sync_versions` toccata da un'operazione e il suo istante.
fn versioned_entity(operation: &mut SyncOperation) -> Option<(String, &mut u64)> {
    match operation {
        SyncOperation::RecordEvent(_)
        | SyncOperation::DeleteEvent { .. }
        | SyncOperation::TombstoneEvent { .. } => None,
        SyncOperation::UpsertContact {
            contact,
            changed_at_ms,
        } => Some((format!("contact:{}", contact.peer), changed_at_ms)),
        SyncOperation::DeleteContact {
            peer,
            changed_at_ms,
        } => Some((format!("contact:{peer}"), changed_at_ms)),
        SyncOperation::ClearConversation {
            peer,
            changed_at_ms,
        } => Some((format!("conversation:{peer}"), changed_at_ms)),
        SyncOperation::UpsertGroup {
            group,
            changed_at_ms,
        } => Some((format!("group:{}", group.id), changed_at_ms)),
        SyncOperation::DeleteGroup { id, changed_at_ms } => {
            Some((format!("group:{id}"), changed_at_ms))
        }
    }
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

    /// Audit #11: oltre gli ultimi 100 messaggi la cronologia non era
    /// raggiungibile. Il cursore `before` scorre le pagine fino all'inizio.
    #[test]
    fn conversation_pages_reach_the_oldest_message() {
        let path = std::env::temp_dir().join(format!("msnnext-pages-{}.db", std::process::id()));
        fs::remove_file(&path).ok();
        let history = History::open(&path, [5; 32]).unwrap();
        for index in 0..250u64 {
            history
                .record_with_id(
                    &format!("id-{index:03}"),
                    "alice",
                    "in",
                    "text",
                    &format!("m{index}"),
                    index,
                )
                .unwrap();
            history.record("bob", "in", "text", "altro", index).unwrap();
        }
        let mut before: Option<String> = None;
        let mut seen = Vec::new();
        loop {
            let page = history
                .conversation_before("alice", before.as_deref(), 100)
                .unwrap();
            if page.is_empty() {
                break;
            }
            assert!(page.iter().all(|entry| entry.peer == "alice"));
            before = Some(page.last().unwrap().event_id.clone());
            seen.extend(page.into_iter().map(|entry| entry.body));
        }
        assert_eq!(seen.len(), 250);
        assert_eq!(seen.first().map(String::as_str), Some("m249"));
        assert_eq!(seen.last().map(String::as_str), Some("m0"));
        fs::remove_file(path).ok();
    }

    fn logged_bodies(history: &History) -> Vec<String> {
        history
            .sync_records_since(0, usize::MAX >> 1)
            .unwrap()
            .into_iter()
            .filter_map(|record| match record.operation {
                SyncOperation::RecordEvent(event) => Some(event.body),
                _ => None,
            })
            .collect()
    }

    /// Audit #17: un messaggio cancellato spariva da `events` ma la copia
    /// restava nel sync_log, reinviato a ogni nuovo dispositivo.
    /// Audit #24: con l'orologio indietro una modifica locale fatta dopo
    /// averne ricevuta una remota veniva scartata (last-write-wins).
    #[test]
    fn local_changes_win_over_seen_ones_despite_clock_skew() {
        let path = std::env::temp_dir().join(format!("msnnext-hlc-{}.db", std::process::id()));
        fs::remove_file(&path).ok();
        let history = History::open(&path, [7; 32]).unwrap();
        let ahead = now_ms() + 60 * 60 * 1000;
        history
            .apply_sync_records(&[SyncRecord {
                seq: 1,
                op_id: "remote".into(),
                operation: SyncOperation::UpsertContact {
                    contact: ContactEntry {
                        peer: "alice".into(),
                        name: "Alice (altro PC)".into(),
                        link: String::new(),
                        account_id: String::new(),
                    },
                    changed_at_ms: ahead,
                },
            }])
            .unwrap();

        history.rename_contact("alice", "Alice").unwrap();
        assert_eq!(history.contacts().unwrap()[0].name, "Alice");

        // L'altro dispositivo riceve la rinomina e la applica a sua volta.
        let other_path =
            std::env::temp_dir().join(format!("msnnext-hlc-other-{}.db", std::process::id()));
        fs::remove_file(&other_path).ok();
        let other = History::open(&other_path, [8; 32]).unwrap();
        other
            .apply_sync_records(&history.sync_records_since(0, 100).unwrap())
            .unwrap();
        assert_eq!(other.contacts().unwrap()[0].name, "Alice");
        fs::remove_file(path).ok();
        fs::remove_file(other_path).ok();
    }

    #[test]
    fn deleted_messages_leave_the_sync_log() {
        let path = std::env::temp_dir().join(format!("msnnext-redact-{}.db", std::process::id()));
        fs::remove_file(&path).ok();
        let history = History::open(&path, [6; 32]).unwrap();
        history
            .record_with_id("a1", "alice", "in", "text", "per me", 10)
            .unwrap();
        history
            .record_with_id("a2", "alice", "out", "text", "per tutti", 11)
            .unwrap();
        history
            .record_with_id("b1", "bob", "in", "text", "svuotato", 12)
            .unwrap();
        history
            .record_with_id("c1", "carol", "in", "text", "contatto eliminato", 13)
            .unwrap();
        history
            .record_with_id("a3", "alice", "in", "text", "resta", 14)
            .unwrap();

        history.delete_event("a1", 20).unwrap();
        history.tombstone_event("a2", 21).unwrap();
        history.clear_conversation("bob").unwrap();
        history.delete_contact("carol").unwrap();
        assert_eq!(
            logged_bodies(&history),
            vec!["".to_owned(), "resta".to_owned()]
        );

        // Lo stesso messaggio che arriva dopo da un altro dispositivo non
        // rientra nel log con il testo.
        let late = SyncRecord {
            seq: 1,
            op_id: "remoto".into(),
            operation: SyncOperation::RecordEvent(Entry {
                event_id: "a1".into(),
                peer: "alice".into(),
                direction: "in".into(),
                kind: "text".into(),
                body: "per me".into(),
                timestamp_ms: 10,
            }),
        };
        history.apply_sync_records(&[late]).unwrap();
        assert!(!logged_bodies(&history).contains(&"per me".to_owned()));
        assert!(history.latest_sync_seq().unwrap() >= 9);
        fs::remove_file(path).ok();
    }

    /// I database precedenti all'indice vengono compattati una volta
    /// all'apertura.
    #[test]
    fn old_sync_log_is_compacted_on_open() {
        let path = std::env::temp_dir().join(format!("msnnext-compact-{}.db", std::process::id()));
        fs::remove_file(&path).ok();
        {
            let history = History::open(&path, [4; 32]).unwrap();
            history
                .record_with_id("x1", "alice", "in", "text", "segreto", 10)
                .unwrap();
            history
                .record_with_id("x2", "alice", "in", "text", "resta", 11)
                .unwrap();
            // Come una cancellazione fatta da una versione precedente: il
            // testo resta nel log e il log non ha le colonne nuove.
            history
                .connection
                .execute_batch(
                    "DELETE FROM events WHERE event_id = 'x1';
                     INSERT INTO deleted_events (event_id, tombstone, changed_at_ms) VALUES ('x1', 0, 20);
                     DROP INDEX sync_log_event_id;
                     DROP INDEX sync_log_peer;
                     ALTER TABLE sync_log DROP COLUMN event_id;
                     ALTER TABLE sync_log DROP COLUMN peer;",
                )
                .unwrap();
        }
        let history = History::open(&path, [4; 32]).unwrap();
        assert_eq!(logged_bodies(&history), vec!["resta".to_owned()]);
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
    fn merging_two_contacts_moves_the_chat_and_keeps_the_account() {
        let path =
            std::env::temp_dir().join(format!("msnnext-contact-merge-{}.db", std::process::id()));
        fs::remove_file(&path).ok();
        let history = History::open(&path, [11; 32]).unwrap();
        history
            .save_contact("peer-a", "Phillip", "link", 1)
            .unwrap();
        history.save_contact("peer-b", "Amico", "", 2).unwrap();
        history.record("peer-a", "in", "text", "sul pc", 3).unwrap();
        history
            .record("peer-b", "in", "text", "dal telefono", 4)
            .unwrap();

        history
            .bind_contact_account("peer-a", "account-1", 5)
            .unwrap();
        // Un rename successivo non deve perdere il legame con l'account.
        history.rename_contact("peer-a", "Phil").unwrap();
        let contact = history
            .contacts()
            .unwrap()
            .into_iter()
            .find(|contact| contact.peer == "peer-a")
            .unwrap();
        assert_eq!(contact.account_id, "account-1");
        assert_eq!(contact.name, "Phil");
        assert_eq!(contact.link, "link");

        assert_eq!(history.merge_contact("peer-b", "peer-a", 6).unwrap(), 1);
        let contacts = history.contacts().unwrap();
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].peer, "peer-a");
        let conversation = history.conversation("peer-a", 10).unwrap();
        assert_eq!(conversation.len(), 2);
        assert!(conversation
            .iter()
            .any(|entry| entry.body == "dal telefono"));
        assert!(history.conversation("peer-b", 10).unwrap().is_empty());
        // Il duplicato è un dispositivo del contatto, non un bloccato.
        assert!(history.ignored_contacts().unwrap().is_empty());

        // La fusione viaggia nel log di sincronizzazione.
        let twin_path = std::env::temp_dir().join(format!(
            "msnnext-contact-merge-twin-{}.db",
            std::process::id()
        ));
        fs::remove_file(&twin_path).ok();
        let twin = History::open(&twin_path, [11; 32]).unwrap();
        twin.apply_sync_records(&history.sync_records_since(0, 500).unwrap())
            .unwrap();
        assert_eq!(twin.contacts().unwrap().len(), 1);
        assert_eq!(twin.conversation("peer-a", 10).unwrap().len(), 2);

        drop(history);
        drop(twin);
        fs::remove_file(path).ok();
        fs::remove_file(twin_path).ok();
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
