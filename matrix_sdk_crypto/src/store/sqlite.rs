// Copyright 2020 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    convert::TryFrom,
    path::Path,
    result::Result as StdResult,
    sync::{Arc, Mutex as SyncMutex},
};

use dashmap::DashSet;
use matrix_sdk_common::{
    api::r0::keys::{CrossSigningKey, KeyUsage},
    async_trait,
    identifiers::{
        DeviceId, DeviceIdBox, DeviceKeyAlgorithm, DeviceKeyId, EventEncryptionAlgorithm, RoomId,
        UserId,
    },
    instant::Duration,
    locks::Mutex,
};
use sqlx::{query, query_as, sqlite::SqliteConnectOptions, Connection, Executor, SqliteConnection};

use super::{
    caches::SessionStore,
    pickle_key::{EncryptedPickleKey, PickleKey},
    Changes, CryptoStore, CryptoStoreError, Result,
};
use crate::{
    identities::{LocalTrust, OwnUserIdentity, ReadOnlyDevice, UserIdentities, UserIdentity},
    olm::{
        AccountPickle, IdentityKeys, InboundGroupSession, InboundGroupSessionPickle,
        OlmMessageHash, PickledAccount, PickledCrossSigningIdentity, PickledInboundGroupSession,
        PickledSession, PicklingMode, PrivateCrossSigningIdentity, ReadOnlyAccount, Session,
        SessionPickle,
    },
};

/// This needs to be 32 bytes long since AES-GCM requires it, otherwise we will
/// panic once we try to pickle a Signing object.
const DEFAULT_PICKLE: &str = "DEFAULT_PICKLE_PASSPHRASE_123456";

/// SQLite based implementation of a `CryptoStore`.
#[derive(Clone)]
#[cfg_attr(feature = "docs", doc(cfg(r#sqlite_cryptostore)))]
pub struct SqliteStore {
    user_id: Arc<UserId>,
    device_id: Arc<Box<DeviceId>>,
    account_info: Arc<SyncMutex<Option<AccountInfo>>>,
    path: Arc<Path>,

    sessions: SessionStore,
    tracked_users: Arc<DashSet<UserId>>,
    users_for_key_query: Arc<DashSet<UserId>>,

    connection: Arc<Mutex<SqliteConnection>>,
    pickle_key: Arc<PickleKey>,
}

#[derive(Clone)]
struct AccountInfo {
    account_id: i64,
    identity_keys: Arc<IdentityKeys>,
}

#[derive(Debug, PartialEq, Copy, Clone, sqlx::Type)]
#[repr(i32)]
enum CrosssigningKeyType {
    Master = 0,
    SelfSigning = 1,
    UserSigning = 2,
}

static DATABASE_NAME: &str = "matrix-sdk-crypto.db";

impl SqliteStore {
    /// Open a new `SqliteStore`.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user for which the store should be
    /// opened.
    ///
    /// * `device_id` - The unique id of the device for which the store should
    /// be opened.
    ///
    /// * `path` - The path where the database file should reside in.
    pub async fn open<P: AsRef<Path>>(
        user_id: &UserId,
        device_id: &DeviceId,
        path: P,
    ) -> Result<SqliteStore> {
        SqliteStore::open_helper(user_id, device_id, path, None).await
    }

    /// Open a new `SqliteStore`.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user for which the store should be
    /// opened.
    ///
    /// * `device_id` - The unique id of the device for which the store should
    /// be opened.
    ///
    /// * `path` - The path where the database file should reside in.
    ///
    /// * `passphrase` - The passphrase that should be used to securely store
    /// the encryption keys.
    pub async fn open_with_passphrase<P: AsRef<Path>>(
        user_id: &UserId,
        device_id: &DeviceId,
        path: P,
        passphrase: &str,
    ) -> Result<SqliteStore> {
        SqliteStore::open_helper(user_id, device_id, path, Some(passphrase)).await
    }

    async fn open_helper<P: AsRef<Path>>(
        user_id: &UserId,
        device_id: &DeviceId,
        path: P,
        passphrase: Option<&str>,
    ) -> Result<SqliteStore> {
        let path = path.as_ref().join(DATABASE_NAME);
        let options = SqliteConnectOptions::new()
            .foreign_keys(true)
            .create_if_missing(true)
            .read_only(false)
            .filename(&path);

        let mut connection = SqliteConnection::connect_with(&options).await?;
        Self::create_tables(&mut connection).await?;

        let pickle_key = if let Some(passphrase) = passphrase {
            Self::get_or_create_pickle_key(user_id, device_id, &passphrase, &mut connection).await?
        } else {
            PickleKey::try_from(DEFAULT_PICKLE.as_bytes().to_vec())
                .expect("Can't create default pickle key")
        };

        let store = SqliteStore {
            user_id: Arc::new(user_id.to_owned()),
            device_id: Arc::new(device_id.into()),
            account_info: Arc::new(SyncMutex::new(None)),
            sessions: SessionStore::new(),
            path: path.into(),
            connection: Arc::new(Mutex::new(connection)),
            tracked_users: Arc::new(DashSet::new()),
            users_for_key_query: Arc::new(DashSet::new()),
            pickle_key: Arc::new(pickle_key),
        };

        Ok(store)
    }

    fn account_id(&self) -> Option<i64> {
        self.account_info
            .lock()
            .unwrap()
            .as_ref()
            .map(|i| i.account_id)
    }

    async fn create_tables(connection: &mut SqliteConnection) -> Result<()> {
        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS accounts (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "user_id" TEXT NOT NULL,
                "device_id" TEXT NOT NULL,
                "pickle" BLOB NOT NULL,
                "shared" INTEGER NOT NULL,
                "uploaded_key_count" INTEGER NOT NULL,
                UNIQUE(user_id,device_id)
            );
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS private_identities (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "account_id" INTEGER NOT NULL,
                "user_id" TEXT NOT NULL,
                "pickle" TEXT NOT NULL,
                "shared" INTEGER NOT NULL,
                FOREIGN KEY ("account_id") REFERENCES "accounts" ("id")
                    ON DELETE CASCADE
                UNIQUE(account_id, user_id)
            );
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS pickle_keys (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "user_id" TEXT NOT NULL,
                "device_id" TEXT NOT NULL,
                "key" TEXT NOT NULL,
                UNIQUE(user_id,device_id)
            );
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS sessions (
                "session_id" TEXT NOT NULL PRIMARY KEY,
                "account_id" INTEGER NOT NULL,
                "creation_time" TEXT NOT NULL,
                "last_use_time" TEXT NOT NULL,
                "sender_key" TEXT NOT NULL,
                "pickle" BLOB NOT NULL,
                FOREIGN KEY ("account_id") REFERENCES "accounts" ("id")
                    ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS "olmsessions_account_id" ON "sessions" ("account_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS tracked_users (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "account_id" INTEGER NOT NULL,
                "user_id" TEXT NOT NULL,
                "dirty" INTEGER NOT NULL,
                FOREIGN KEY ("account_id") REFERENCES "accounts" ("id")
                    ON DELETE CASCADE
                UNIQUE(account_id,user_id)
            );

            CREATE INDEX IF NOT EXISTS "tracked_users_account_id" ON "tracked_users" ("account_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS inbound_group_sessions (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "session_id" TEXT NOT NULL,
                "account_id" INTEGER NOT NULL,
                "sender_key" TEXT NOT NULL,
                "room_id" TEXT NOT NULL,
                "pickle" BLOB NOT NULL,
                "imported" INTEGER NOT NULL,
                FOREIGN KEY ("account_id") REFERENCES "accounts" ("id")
                    ON DELETE CASCADE
                UNIQUE(account_id,session_id,sender_key)
            );

            CREATE INDEX IF NOT EXISTS "olm_groups_sessions_account_id" ON "inbound_group_sessions" ("account_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS group_session_claimed_keys (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "session_id" INTEGER NOT NULL,
                "algorithm" TEXT NOT NULL,
                "key" TEXT NOT NULL,
                FOREIGN KEY ("session_id") REFERENCES "inbound_group_sessions" ("id")
                    ON DELETE CASCADE
                UNIQUE(session_id, algorithm)
            );

            CREATE INDEX IF NOT EXISTS "group_session_claimed_keys_session_id" ON "group_session_claimed_keys" ("session_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS group_session_chains (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "key" TEXT NOT NULL,
                "session_id" INTEGER NOT NULL,
                FOREIGN KEY ("session_id") REFERENCES "inbound_group_sessions" ("id")
                    ON DELETE CASCADE
                UNIQUE(session_id, key)
            );

            CREATE INDEX IF NOT EXISTS "group_session_chains_session_id" ON "group_session_chains" ("session_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS devices (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "account_id" INTEGER NOT NULL,
                "user_id" TEXT NOT NULL,
                "device_id" TEXT NOT NULL,
                "display_name" TEXT,
                "trust_state" INTEGER NOT NULL,
                FOREIGN KEY ("account_id") REFERENCES "accounts" ("id")
                    ON DELETE CASCADE
                UNIQUE(account_id,user_id,device_id)
            );

            CREATE INDEX IF NOT EXISTS "devices_account_id" ON "devices" ("account_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS algorithms (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "device_id" INTEGER NOT NULL,
                "algorithm" TEXT NOT NULL,
                FOREIGN KEY ("device_id") REFERENCES "devices" ("id")
                    ON DELETE CASCADE
                UNIQUE(device_id, algorithm)
            );

            CREATE INDEX IF NOT EXISTS "algorithms_device_id" ON "algorithms" ("device_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS device_keys (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "device_id" INTEGER NOT NULL,
                "algorithm" TEXT NOT NULL,
                "key" TEXT NOT NULL,
                FOREIGN KEY ("device_id") REFERENCES "devices" ("id")
                    ON DELETE CASCADE
                UNIQUE(device_id, algorithm)
            );

            CREATE INDEX IF NOT EXISTS "device_keys_device_id" ON "device_keys" ("device_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS device_signatures (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "device_id" INTEGER NOT NULL,
                "user_id" TEXT NOT NULL,
                "key_algorithm" TEXT NOT NULL,
                "signature" TEXT NOT NULL,
                FOREIGN KEY ("device_id") REFERENCES "devices" ("id")
                    ON DELETE CASCADE
                UNIQUE(device_id, user_id, key_algorithm)
            );

            CREATE INDEX IF NOT EXISTS "device_keys_device_id" ON "device_keys" ("device_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS users (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "account_id" INTEGER NOT NULL,
                "user_id" TEXT NOT NULL,
                FOREIGN KEY ("account_id") REFERENCES "accounts" ("id")
                    ON DELETE CASCADE
                UNIQUE(account_id,user_id)
            );

            CREATE INDEX IF NOT EXISTS "users_account_id" ON "users" ("account_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS users_trust_state (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "trusted" INTEGER NOT NULL,
                "user_id" INTEGER NOT NULL,
                FOREIGN KEY ("user_id") REFERENCES "users" ("id")
                    ON DELETE CASCADE
                UNIQUE(user_id)
            );
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS cross_signing_keys (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "key_type" INTEGER NOT NULL,
                "usage" STRING NOT NULL,
                "user_id" INTEGER NOT NULL,
                FOREIGN KEY ("user_id") REFERENCES "users" ("id") ON DELETE CASCADE
                UNIQUE(user_id, key_type)
            );

            CREATE INDEX IF NOT EXISTS "cross_signing_keys_users" ON "users" ("user_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS user_keys (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "key" TEXT NOT NULL,
                "key_id" TEXT NOT NULL,
                "cross_signing_key" INTEGER NOT NULL,
                FOREIGN KEY ("cross_signing_key") REFERENCES "cross_signing_keys" ("id") ON DELETE CASCADE
                UNIQUE(cross_signing_key, key_id)
            );

            CREATE INDEX IF NOT EXISTS "cross_signing_keys_keys" ON "cross_signing_keys" ("cross_signing_key");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS user_key_signatures (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "user_id" TEXT NOT NULL,
                "key_id" INTEGER NOT NULL,
                "signature" TEXT NOT NULL,
                "cross_signing_key" INTEGER NOT NULL,
                FOREIGN KEY ("cross_signing_key") REFERENCES "cross_signing_keys" ("id")
                    ON DELETE CASCADE
                UNIQUE(user_id, key_id, cross_signing_key)
            );

            CREATE INDEX IF NOT EXISTS "cross_signing_keys_signatures" ON "cross_signing_keys" ("cross_signing_key");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS key_value (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "account_id" INTEGER NOT NULL,
                "key" TEXT NOT NULL,
                "value" TEXT NOT NULL,
                FOREIGN KEY ("account_id") REFERENCES "accounts" ("id")
                    ON DELETE CASCADE
                UNIQUE(account_id,key)
            );

            CREATE INDEX IF NOT EXISTS "key_values_index" ON "key_value" ("account_id");
        "#,
            )
            .await?;

        connection
            .execute(
                r#"
            CREATE TABLE IF NOT EXISTS olm_hashes (
                "id" INTEGER NOT NULL PRIMARY KEY,
                "account_id" INTEGER NOT NULL,
                "sender_key" TEXT NOT NULL,
                "hash" TEXT NOT NULL,
                FOREIGN KEY ("account_id") REFERENCES "accounts" ("id")
                    ON DELETE CASCADE
                UNIQUE(account_id,sender_key,hash)
            );

            CREATE INDEX IF NOT EXISTS "olm_hashes_index" ON "olm_hashes" ("account_id");
        "#,
            )
            .await?;

        Ok(())
    }

    async fn save_pickle_key(
        user_id: &UserId,
        device_id: &DeviceId,
        key: EncryptedPickleKey,
        connection: &mut SqliteConnection,
    ) -> Result<()> {
        let key = serde_json::to_string(&key)?;

        query(
            "INSERT INTO pickle_keys (
                user_id, device_id, key
             ) VALUES (?1, ?2, ?3)
             ON CONFLICT(user_id, device_id) DO UPDATE SET
                key = excluded.key
             ",
        )
        .bind(user_id.as_str())
        .bind(device_id.as_str())
        .bind(key)
        .execute(&mut *connection)
        .await?;

        Ok(())
    }

    async fn get_or_create_pickle_key(
        user_id: &UserId,
        device_id: &DeviceId,
        passphrase: &str,
        connection: &mut SqliteConnection,
    ) -> Result<PickleKey> {
        let row: Option<(String,)> =
            query_as("SELECT key FROM pickle_keys WHERE user_id = ? and device_id = ?")
                .bind(user_id.as_str())
                .bind(device_id.as_str())
                .fetch_optional(&mut *connection)
                .await?;

        Ok(if let Some(row) = row {
            let encrypted: EncryptedPickleKey = serde_json::from_str(&row.0)?;
            PickleKey::from_encrypted(passphrase, encrypted)
                .map_err(|_| CryptoStoreError::UnpicklingError)?
        } else {
            let key = PickleKey::new();
            let encrypted = key.encrypt(passphrase);
            Self::save_pickle_key(user_id, device_id, encrypted, connection).await?;

            key
        })
    }

    async fn lazy_load_sessions(
        &self,
        connection: &mut SqliteConnection,
        sender_key: &str,
    ) -> Result<()> {
        let loaded_sessions = self.sessions.get(sender_key).is_some();

        if !loaded_sessions {
            let sessions = self
                .load_sessions_for_helper(connection, sender_key)
                .await?;

            if !sessions.is_empty() {
                self.sessions.set_for_sender(sender_key, sessions);
            }
        }

        Ok(())
    }

    async fn get_sessions_for(
        &self,
        connection: &mut SqliteConnection,
        sender_key: &str,
    ) -> Result<Option<Arc<Mutex<Vec<Session>>>>> {
        self.lazy_load_sessions(connection, sender_key).await?;
        Ok(self.sessions.get(sender_key))
    }

    #[cfg(test)]
    async fn load_sessions_for(&self, sender_key: &str) -> Result<Vec<Session>> {
        let mut connection = self.connection.lock().await;
        self.load_sessions_for_helper(&mut connection, sender_key)
            .await
    }

    async fn load_sessions_for_helper(
        &self,
        connection: &mut SqliteConnection,
        sender_key: &str,
    ) -> Result<Vec<Session>> {
        let account_info = self
            .account_info
            .lock()
            .unwrap()
            .clone()
            .ok_or(CryptoStoreError::AccountUnset)?;
        let mut rows: Vec<(String, String, String, String)> = query_as(
            "SELECT pickle, sender_key, creation_time, last_use_time
             FROM sessions WHERE account_id = ? and sender_key = ?",
        )
        .bind(account_info.account_id)
        .bind(sender_key)
        .fetch_all(&mut *connection)
        .await?;

        Ok(rows
            .drain(..)
            .map(|row| {
                let pickle = row.0;
                let sender_key = row.1;
                let creation_time = serde_json::from_str::<Duration>(&row.2)?;
                let last_use_time = serde_json::from_str::<Duration>(&row.3)?;

                let pickle = PickledSession {
                    pickle: SessionPickle::from(pickle),
                    last_use_time,
                    creation_time,
                    sender_key,
                };

                Ok(Session::from_pickle(
                    self.user_id.clone(),
                    self.device_id.clone(),
                    account_info.identity_keys.clone(),
                    pickle,
                    self.get_pickle_mode(),
                )?)
            })
            .collect::<Result<Vec<Session>>>()?)
    }

    async fn load_inbound_session_data(
        &self,
        connection: &mut SqliteConnection,
        session_row_id: i64,
        pickle: String,
        sender_key: String,
        room_id: RoomId,
        imported: bool,
    ) -> Result<InboundGroupSession> {
        let key_rows: Vec<(String, String)> =
            query_as("SELECT algorithm, key FROM group_session_claimed_keys WHERE session_id = ?")
                .bind(session_row_id)
                .fetch_all(&mut *connection)
                .await?;

        let claimed_keys: BTreeMap<DeviceKeyAlgorithm, String> = key_rows
            .into_iter()
            .filter_map(|row| {
                let algorithm = DeviceKeyAlgorithm::try_from(row.0).ok()?;
                let key = row.1;

                Some((algorithm, key))
            })
            .collect();

        let mut chain_rows: Vec<(String,)> =
            query_as("SELECT key, key FROM group_session_chains WHERE session_id = ?")
                .bind(session_row_id)
                .fetch_all(&mut *connection)
                .await?;

        let chains: Vec<String> = chain_rows.drain(..).map(|r| r.0).collect();

        let chains = if chains.is_empty() {
            None
        } else {
            Some(chains)
        };

        let pickle = PickledInboundGroupSession {
            pickle: InboundGroupSessionPickle::from(pickle),
            sender_key,
            signing_key: claimed_keys,
            room_id,
            forwarding_chains: chains,
            imported,
        };

        Ok(InboundGroupSession::from_pickle(
            pickle,
            self.get_pickle_mode(),
        )?)
    }

    async fn load_inbound_group_session_helper(
        &self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let row: Option<(i64, String, bool)> = query_as(
            "SELECT id, pickle, imported
             FROM inbound_group_sessions
             WHERE (
                 account_id = ? and
                 room_id = ? and
                 sender_key = ? and
                 session_id = ?
             )",
        )
        .bind(account_id)
        .bind(room_id.as_str())
        .bind(sender_key)
        .bind(session_id)
        .fetch_optional(&mut *connection)
        .await?;

        let row = if let Some(r) = row {
            r
        } else {
            return Ok(None);
        };

        let session_row_id = row.0;
        let pickle = row.1;
        let imported = row.2;

        let session = self
            .load_inbound_session_data(
                &mut connection,
                session_row_id,
                pickle,
                sender_key.to_owned(),
                room_id.to_owned(),
                imported,
            )
            .await?;

        Ok(Some(session))
    }

    async fn load_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>> {
        let mut sessions = Vec::new();

        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let mut rows: Vec<(i64, String, String, String, bool)> = query_as(
            "SELECT id, pickle, sender_key, room_id, imported
             FROM inbound_group_sessions WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_all(&mut *connection)
        .await?;

        for row in rows.drain(..) {
            let session_row_id = row.0;
            let pickle = row.1;
            let sender_key = row.2;
            let room_id = RoomId::try_from(row.3)?;
            let imported = row.4;

            let session = self
                .load_inbound_session_data(
                    &mut connection,
                    session_row_id,
                    pickle,
                    sender_key,
                    room_id.to_owned(),
                    imported,
                )
                .await?;

            sessions.push(session);
        }

        Ok(sessions)
    }

    async fn save_tracked_user(&self, user: &UserId, dirty: bool) -> Result<()> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;
        // TODO see the todo in the memory store, we need to avoid a race
        // between a sync and key query.

        query(
            "INSERT INTO tracked_users (
                account_id, user_id, dirty
             ) VALUES (?1, ?2, ?3)
             ON CONFLICT(account_id, user_id) DO UPDATE SET
                dirty = excluded.dirty
             ",
        )
        .bind(account_id)
        .bind(user.to_string())
        .bind(dirty)
        .execute(&mut *connection)
        .await?;

        Ok(())
    }

    async fn load_tracked_users(&self) -> Result<()> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let rows: Vec<(String, bool)> = query_as(
            "SELECT user_id, dirty
             FROM tracked_users WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_all(&mut *connection)
        .await?;

        for row in rows {
            let user_id: &str = &row.0;
            let dirty: bool = row.1;

            if let Ok(u) = UserId::try_from(user_id) {
                self.tracked_users.insert(u.clone());
                if dirty {
                    self.users_for_key_query.insert(u);
                }
            } else {
                continue;
            };
        }

        Ok(())
    }

    async fn load_device_data(
        &self,
        connection: &mut SqliteConnection,
        device_row_id: i64,
        user_id: &UserId,
        device_id: DeviceIdBox,
        trust_state: LocalTrust,
        display_name: Option<String>,
    ) -> Result<ReadOnlyDevice> {
        let algorithm_rows: Vec<(String,)> =
            query_as("SELECT algorithm FROM algorithms WHERE device_id = ?")
                .bind(device_row_id)
                .fetch_all(&mut *connection)
                .await?;

        let algorithms = algorithm_rows
            .iter()
            .map(|row| {
                let algorithm: &str = &row.0;
                EventEncryptionAlgorithm::from(algorithm)
            })
            .collect::<Vec<EventEncryptionAlgorithm>>();

        let key_rows: Vec<(String, String)> =
            query_as("SELECT algorithm, key FROM device_keys WHERE device_id = ?")
                .bind(device_row_id)
                .fetch_all(&mut *connection)
                .await?;

        let keys: BTreeMap<DeviceKeyId, String> = key_rows
            .into_iter()
            .filter_map(|row| {
                let algorithm = DeviceKeyAlgorithm::try_from(row.0).ok()?;
                let key = row.1;

                Some((DeviceKeyId::from_parts(algorithm, &device_id), key))
            })
            .collect();

        let signature_rows: Vec<(String, String, String)> = query_as(
            "SELECT user_id, key_algorithm, signature
                     FROM device_signatures WHERE device_id = ?",
        )
        .bind(device_row_id)
        .fetch_all(&mut *connection)
        .await?;

        let mut signatures: BTreeMap<UserId, BTreeMap<DeviceKeyId, String>> = BTreeMap::new();

        for row in signature_rows {
            let user_id = if let Ok(u) = UserId::try_from(&*row.0) {
                u
            } else {
                continue;
            };

            let key_algorithm = if let Ok(k) = DeviceKeyAlgorithm::try_from(row.1) {
                k
            } else {
                continue;
            };

            let signature = row.2;

            signatures
                .entry(user_id)
                .or_insert_with(BTreeMap::new)
                .insert(
                    DeviceKeyId::from_parts(key_algorithm, device_id.as_str().into()),
                    signature.to_owned(),
                );
        }

        Ok(ReadOnlyDevice::new(
            user_id.to_owned(),
            device_id,
            display_name.clone(),
            trust_state,
            algorithms,
            keys,
            signatures,
        ))
    }

    async fn get_single_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<ReadOnlyDevice>> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let row: Option<(i64, Option<String>, i64)> = query_as(
            "SELECT id, display_name, trust_state
             FROM devices WHERE account_id = ? and user_id = ? and device_id = ?",
        )
        .bind(account_id)
        .bind(user_id.as_str())
        .bind(device_id.as_str())
        .fetch_optional(&mut *connection)
        .await?;

        let row = if let Some(r) = row {
            r
        } else {
            return Ok(None);
        };

        let device_row_id = row.0;
        let display_name = row.1;
        let trust_state = LocalTrust::from(row.2);
        let device = self
            .load_device_data(
                &mut connection,
                device_row_id,
                user_id,
                device_id.into(),
                trust_state,
                display_name,
            )
            .await?;

        Ok(Some(device))
    }

    async fn load_devices(&self, user_id: &UserId) -> Result<HashMap<DeviceIdBox, ReadOnlyDevice>> {
        let mut devices = HashMap::new();

        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let mut rows: Vec<(i64, String, Option<String>, i64)> = query_as(
            "SELECT id, device_id, display_name, trust_state
             FROM devices WHERE account_id = ? and user_id = ?",
        )
        .bind(account_id)
        .bind(user_id.as_str())
        .fetch_all(&mut *connection)
        .await?;

        for row in rows.drain(..) {
            let device_row_id = row.0;
            let device_id: DeviceIdBox = row.1.into();
            let display_name = row.2;
            let trust_state = LocalTrust::from(row.3);

            let device = self
                .load_device_data(
                    &mut connection,
                    device_row_id,
                    user_id,
                    device_id.clone(),
                    trust_state,
                    display_name,
                )
                .await?;

            devices.insert(device_id, device);
        }

        Ok(devices)
    }

    async fn save_device_helper(
        &self,
        connection: &mut SqliteConnection,
        device: ReadOnlyDevice,
    ) -> Result<()> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;

        query(
            "INSERT INTO devices (
                account_id, user_id, device_id,
                display_name, trust_state
             ) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(account_id, user_id, device_id) DO UPDATE SET
                display_name = excluded.display_name,
                trust_state = excluded.trust_state
             ",
        )
        .bind(account_id)
        .bind(device.user_id().as_str())
        .bind(device.device_id().as_str())
        .bind(device.display_name())
        .bind(device.local_trust_state() as i64)
        .execute(&mut *connection)
        .await?;

        let row: (i64,) = query_as(
            "SELECT id FROM devices
                      WHERE user_id = ? and device_id = ?",
        )
        .bind(device.user_id().as_str())
        .bind(device.device_id().as_str())
        .fetch_one(&mut *connection)
        .await?;

        let device_row_id = row.0;

        for algorithm in device.algorithms() {
            query(
                "INSERT OR IGNORE INTO algorithms (
                    device_id, algorithm
                 ) VALUES (?1, ?2)
                 ",
            )
            .bind(device_row_id)
            .bind(algorithm.to_string())
            .execute(&mut *connection)
            .await?;
        }

        for (key_id, key) in device.keys() {
            query(
                "INSERT OR IGNORE INTO device_keys (
                    device_id, algorithm, key
                 ) VALUES (?1, ?2, ?3)
                 ",
            )
            .bind(device_row_id)
            .bind(key_id.algorithm().to_string())
            .bind(key)
            .execute(&mut *connection)
            .await?;
        }

        for (user_id, signature_map) in device.signatures() {
            for (key_id, signature) in signature_map {
                query(
                    "INSERT OR IGNORE INTO device_signatures (
                        device_id, user_id, key_algorithm, signature
                     ) VALUES (?1, ?2, ?3, ?4)
                     ",
                )
                .bind(device_row_id)
                .bind(user_id.as_str())
                .bind(key_id.algorithm().to_string())
                .bind(signature)
                .execute(&mut *connection)
                .await?;
            }
        }

        Ok(())
    }

    fn get_pickle_mode(&self) -> PicklingMode {
        self.pickle_key.pickle_mode()
    }

    fn get_pickle_key(&self) -> &[u8] {
        self.pickle_key.key()
    }

    async fn save_inbound_group_session_helper(
        &self,
        account_id: i64,
        connection: &mut SqliteConnection,
        session: &InboundGroupSession,
    ) -> Result<()> {
        let pickle = session.pickle(self.get_pickle_mode()).await;
        let session_id = session.session_id();

        query(
            "REPLACE INTO inbound_group_sessions (
                session_id, account_id, sender_key,
                room_id, pickle, imported
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ",
        )
        .bind(session_id)
        .bind(account_id)
        .bind(&pickle.sender_key)
        .bind(pickle.room_id.as_str())
        .bind(pickle.pickle.as_str())
        .bind(pickle.imported)
        .execute(&mut *connection)
        .await?;

        let row: (i64,) = query_as(
            "SELECT id FROM inbound_group_sessions
                      WHERE account_id = ? and session_id = ? and sender_key = ?",
        )
        .bind(account_id)
        .bind(session_id)
        .bind(pickle.sender_key)
        .fetch_one(&mut *connection)
        .await?;

        let session_row_id = row.0;

        for (key_id, key) in pickle.signing_key {
            query(
                "REPLACE INTO group_session_claimed_keys (
                    session_id, algorithm, key
                 ) VALUES (?1, ?2, ?3)
                 ",
            )
            .bind(session_row_id)
            .bind(serde_json::to_string(&key_id)?)
            .bind(key)
            .execute(&mut *connection)
            .await?;
        }

        if let Some(chains) = pickle.forwarding_chains {
            for key in chains {
                query(
                    "REPLACE INTO group_session_chains (
                        session_id, key
                     ) VALUES (?1, ?2)
                     ",
                )
                .bind(session_row_id)
                .bind(key)
                .execute(&mut *connection)
                .await?;
            }
        }

        Ok(())
    }

    async fn load_cross_signing_key(
        connection: &mut SqliteConnection,
        user_id: &UserId,
        user_row_id: i64,
        key_type: CrosssigningKeyType,
    ) -> Result<CrossSigningKey> {
        let row: (i64, String) =
            query_as("SELECT id, usage FROM cross_signing_keys WHERE user_id =? and key_type =?")
                .bind(user_row_id)
                .bind(key_type)
                .fetch_one(&mut *connection)
                .await?;

        let key_row_id = row.0;
        let usage: Vec<KeyUsage> = serde_json::from_str(&row.1)?;

        let key_rows: Vec<(String, String)> =
            query_as("SELECT key_id, key FROM user_keys WHERE cross_signing_key = ?")
                .bind(key_row_id)
                .fetch_all(&mut *connection)
                .await?;

        let mut keys = BTreeMap::new();
        let mut signatures = BTreeMap::new();

        for row in key_rows {
            let key_id = row.0;
            let key = row.1;

            keys.insert(key_id, key);
        }

        let mut signature_rows: Vec<(String, String, String)> = query_as(
            "SELECT user_id, key_id, signature FROM user_key_signatures WHERE cross_signing_key = ?",
        )
        .bind(key_row_id)
        .fetch_all(&mut *connection)
        .await?;

        for row in signature_rows.drain(..) {
            let user_id = if let Ok(u) = UserId::try_from(row.0) {
                u
            } else {
                continue;
            };

            let key_id = row.1;
            let signature = row.2;

            signatures
                .entry(user_id)
                .or_insert_with(BTreeMap::new)
                .insert(key_id, signature);
        }

        Ok(CrossSigningKey {
            user_id: user_id.to_owned(),
            usage,
            keys,
            signatures,
        })
    }

    async fn load_user(&self, user_id: &UserId) -> Result<Option<UserIdentities>> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let row: Option<(i64,)> =
            query_as("SELECT id FROM users WHERE account_id = ? and user_id = ?")
                .bind(account_id)
                .bind(user_id.as_str())
                .fetch_optional(&mut *connection)
                .await?;

        let user_row_id = if let Some(row) = row {
            row.0
        } else {
            return Ok(None);
        };

        let master = SqliteStore::load_cross_signing_key(
            &mut connection,
            user_id,
            user_row_id,
            CrosssigningKeyType::Master,
        )
        .await?;
        let self_singing = SqliteStore::load_cross_signing_key(
            &mut connection,
            user_id,
            user_row_id,
            CrosssigningKeyType::SelfSigning,
        )
        .await?;

        if user_id == &*self.user_id {
            let user_signing = SqliteStore::load_cross_signing_key(
                &mut connection,
                user_id,
                user_row_id,
                CrosssigningKeyType::UserSigning,
            )
            .await?;

            let verified: Option<(bool,)> =
                query_as("SELECT trusted FROM users_trust_state WHERE user_id = ?")
                    .bind(user_row_id)
                    .fetch_optional(&mut *connection)
                    .await?;

            let verified = verified.map_or(false, |r| r.0);

            let identity =
                OwnUserIdentity::new(master.into(), self_singing.into(), user_signing.into())
                    .expect("Signature check failed on stored identity");

            if verified {
                identity.mark_as_verified();
            }

            Ok(Some(UserIdentities::Own(identity)))
        } else {
            Ok(Some(UserIdentities::Other(
                UserIdentity::new(master.into(), self_singing.into())
                    .expect("Signature check failed on stored identity"),
            )))
        }
    }

    async fn save_cross_signing_key(
        connection: &mut SqliteConnection,
        user_row_id: i64,
        key_type: CrosssigningKeyType,
        cross_signing_key: impl AsRef<CrossSigningKey>,
    ) -> Result<()> {
        let cross_signing_key: &CrossSigningKey = cross_signing_key.as_ref();

        query(
            "REPLACE INTO cross_signing_keys (
                user_id, key_type, usage
                ) VALUES (?1, ?2, ?3)
              ",
        )
        .bind(user_row_id)
        .bind(key_type)
        .bind(serde_json::to_string(&cross_signing_key.usage)?)
        .execute(&mut *connection)
        .await?;

        let row: (i64,) = query_as(
            "SELECT id FROM cross_signing_keys
                    WHERE user_id = ? and key_type = ?",
        )
        .bind(user_row_id)
        .bind(key_type)
        .fetch_one(&mut *connection)
        .await?;

        let key_row_id = row.0;

        for (key_id, key) in &cross_signing_key.keys {
            query(
                "REPLACE INTO user_keys (
                    cross_signing_key, key_id, key
                 ) VALUES (?1, ?2, ?3)
                 ",
            )
            .bind(key_row_id)
            .bind(key_id.as_str())
            .bind(key)
            .execute(&mut *connection)
            .await?;
        }

        for (user_id, signature_map) in &cross_signing_key.signatures {
            for (key_id, signature) in signature_map {
                query(
                    "REPLACE INTO user_key_signatures (
                        cross_signing_key, user_id, key_id, signature
                     ) VALUES (?1, ?2, ?3, ?4)
                     ",
                )
                .bind(key_row_id)
                .bind(user_id.as_str())
                .bind(key_id.as_str())
                .bind(signature)
                .execute(&mut *connection)
                .await?;
            }
        }

        Ok(())
    }

    #[cfg(test)]
    async fn save_sessions(&self, sessions: &[Session]) -> Result<()> {
        let mut connection = self.connection.lock().await;
        let mut transaction = connection.begin().await?;

        self.save_sessions_helper(&mut transaction, sessions)
            .await?;
        transaction.commit().await?;

        Ok(())
    }

    async fn save_sessions_helper(
        &self,
        connection: &mut SqliteConnection,
        sessions: &[Session],
    ) -> Result<()> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;

        for session in sessions {
            self.lazy_load_sessions(connection, &session.sender_key)
                .await?;
        }

        for session in sessions {
            self.sessions.add(session.clone()).await;

            let pickle = session.pickle(self.get_pickle_mode()).await;

            let session_id = session.session_id();
            let creation_time = serde_json::to_string(&pickle.creation_time)?;
            let last_use_time = serde_json::to_string(&pickle.last_use_time)?;

            query(
                "REPLACE INTO sessions (
                    session_id, account_id, creation_time, last_use_time, sender_key, pickle
                 ) VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&session_id)
            .bind(&account_id)
            .bind(&*creation_time)
            .bind(&*last_use_time)
            .bind(&pickle.sender_key)
            .bind(&pickle.pickle.as_str())
            .execute(&mut *connection)
            .await?;
        }

        Ok(())
    }

    async fn save_devices(
        &self,
        mut connection: &mut SqliteConnection,
        devices: &[ReadOnlyDevice],
    ) -> Result<()> {
        for device in devices {
            self.save_device_helper(&mut connection, device.clone())
                .await?
        }

        Ok(())
    }

    async fn delete_devices(
        &self,
        connection: &mut SqliteConnection,
        devices: &[ReadOnlyDevice],
    ) -> Result<()> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;

        for device in devices {
            query(
                "DELETE FROM devices
                 WHERE account_id = ?1 and user_id = ?2 and device_id = ?3
                 ",
            )
            .bind(account_id)
            .bind(&device.user_id().to_string())
            .bind(device.device_id().as_str())
            .execute(&mut *connection)
            .await?;
        }

        Ok(())
    }

    #[cfg(test)]
    async fn save_inbound_group_sessions_test(
        &self,
        sessions: &[InboundGroupSession],
    ) -> Result<()> {
        let mut connection = self.connection.lock().await;
        let mut transaction = connection.begin().await?;

        self.save_inbound_group_sessions(&mut transaction, sessions)
            .await?;

        transaction.commit().await?;
        Ok(())
    }

    async fn save_inbound_group_sessions(
        &self,
        connection: &mut SqliteConnection,
        sessions: &[InboundGroupSession],
    ) -> Result<()> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;

        for session in sessions {
            self.save_inbound_group_session_helper(account_id, connection, session)
                .await?;
        }

        Ok(())
    }

    async fn save_user_identities(
        &self,
        mut connection: &mut SqliteConnection,
        users: &[UserIdentities],
    ) -> Result<()> {
        for user in users {
            self.save_user_helper(&mut connection, user).await?;
        }
        Ok(())
    }

    async fn save_olm_hashses(
        &self,
        connection: &mut SqliteConnection,
        hashes: &[OlmMessageHash],
    ) -> Result<()> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;

        for hash in hashes {
            query("REPLACE INTO olm_hashes (account_id, sender_key, hash) VALUES (?1, ?2, ?3)")
                .bind(account_id)
                .bind(&hash.sender_key)
                .bind(&hash.hash)
                .execute(&mut *connection)
                .await?;
        }

        Ok(())
    }

    async fn save_identity(
        &self,
        connection: &mut SqliteConnection,
        identity: PrivateCrossSigningIdentity,
    ) -> Result<()> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let pickle = identity.pickle(self.get_pickle_key()).await?;

        query(
            "INSERT INTO private_identities (
                account_id, user_id, pickle, shared
             ) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(account_id, user_id) DO UPDATE SET
                pickle = excluded.pickle,
                shared = excluded.shared
             ",
        )
        .bind(account_id)
        .bind(pickle.user_id.as_str())
        .bind(pickle.pickle)
        .bind(pickle.shared)
        .execute(&mut *connection)
        .await?;

        Ok(())
    }

    async fn save_account_helper(
        &self,
        connection: &mut SqliteConnection,
        account: ReadOnlyAccount,
    ) -> Result<()> {
        let pickle = account.pickle(self.get_pickle_mode()).await;

        query(
            "INSERT INTO accounts (
                user_id, device_id, pickle, shared, uploaded_key_count
             ) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(user_id, device_id) DO UPDATE SET
                pickle = excluded.pickle,
                shared = excluded.shared,
                uploaded_key_count = excluded.uploaded_key_count
             ",
        )
        .bind(pickle.user_id.as_str())
        .bind(pickle.device_id.as_str())
        .bind(pickle.pickle.as_str())
        .bind(pickle.shared)
        .bind(pickle.uploaded_signed_key_count)
        .execute(&mut *connection)
        .await?;

        let account_id: (i64,) =
            query_as("SELECT id FROM accounts WHERE user_id = ? and device_id = ?")
                .bind(self.user_id.as_str())
                .bind(self.device_id.as_str())
                .fetch_one(&mut *connection)
                .await?;

        *self.account_info.lock().unwrap() = Some(AccountInfo {
            account_id: account_id.0,
            identity_keys: account.identity_keys.clone(),
        });

        Ok(())
    }

    async fn save_user_helper(
        &self,
        mut connection: &mut SqliteConnection,
        user: &UserIdentities,
    ) -> Result<()> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;

        query("REPLACE INTO users (account_id, user_id) VALUES (?1, ?2)")
            .bind(account_id)
            .bind(user.user_id().as_str())
            .execute(&mut *connection)
            .await?;

        let row: (i64,) = query_as(
            "SELECT id FROM users
                WHERE account_id = ? and user_id = ?",
        )
        .bind(account_id)
        .bind(user.user_id().as_str())
        .fetch_one(&mut *connection)
        .await?;

        let user_row_id = row.0;

        SqliteStore::save_cross_signing_key(
            &mut connection,
            user_row_id,
            CrosssigningKeyType::Master,
            user.master_key(),
        )
        .await?;
        SqliteStore::save_cross_signing_key(
            &mut connection,
            user_row_id,
            CrosssigningKeyType::SelfSigning,
            user.self_signing_key(),
        )
        .await?;

        if let UserIdentities::Own(own_identity) = user {
            SqliteStore::save_cross_signing_key(
                &mut connection,
                user_row_id,
                CrosssigningKeyType::UserSigning,
                own_identity.user_signing_key(),
            )
            .await?;

            query("REPLACE INTO users_trust_state (user_id, trusted) VALUES (?1, ?2)")
                .bind(user_row_id)
                .bind(own_identity.is_verified())
                .execute(&mut *connection)
                .await?;
        }

        Ok(())
    }
}

#[async_trait]
impl CryptoStore for SqliteStore {
    async fn load_account(&self) -> Result<Option<ReadOnlyAccount>> {
        let mut connection = self.connection.lock().await;

        let row: Option<(i64, String, bool, i64)> = query_as(
            "SELECT id, pickle, shared, uploaded_key_count FROM accounts
                      WHERE user_id = ? and device_id = ?",
        )
        .bind(self.user_id.as_str())
        .bind(self.device_id.as_str())
        .fetch_optional(&mut *connection)
        .await?;

        let result = if let Some((id, pickle, shared, uploaded_key_count)) = row {
            let pickle = PickledAccount {
                user_id: (&*self.user_id).clone(),
                device_id: (&*self.device_id).clone(),
                pickle: AccountPickle::from(pickle),
                shared,
                uploaded_signed_key_count: uploaded_key_count,
            };

            let account = ReadOnlyAccount::from_pickle(pickle, self.get_pickle_mode())?;

            *self.account_info.lock().unwrap() = Some(AccountInfo {
                account_id: id,
                identity_keys: account.identity_keys.clone(),
            });

            Some(account)
        } else {
            return Ok(None);
        };

        drop(connection);

        self.load_tracked_users().await?;

        Ok(result)
    }

    async fn save_account(&self, account: ReadOnlyAccount) -> Result<()> {
        let mut connection = self.connection.lock().await;
        self.save_account_helper(&mut connection, account).await
    }

    async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let row: Option<(String, bool)> = query_as(
            "SELECT pickle, shared FROM private_identities
                      WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_optional(&mut *connection)
        .await?;

        if let Some(row) = row {
            let pickle = PickledCrossSigningIdentity {
                user_id: (&*self.user_id).clone(),
                pickle: row.0,
                shared: row.1,
            };

            // TODO remove this unwrap
            let identity = PrivateCrossSigningIdentity::from_pickle(pickle, self.get_pickle_key())
                .await
                .unwrap();

            Ok(Some(identity))
        } else {
            Ok(None)
        }
    }

    async fn save_changes(&self, changes: Changes) -> Result<()> {
        let mut connection = self.connection.lock().await;
        let mut transaction = connection.begin().await?;

        if let Some(account) = changes.account {
            self.save_account_helper(&mut transaction, account).await?;
        }

        if let Some(identity) = changes.private_identity {
            self.save_identity(&mut transaction, identity).await?;
        }

        self.save_sessions_helper(&mut transaction, &changes.sessions)
            .await?;
        self.save_inbound_group_sessions(&mut transaction, &changes.inbound_group_sessions)
            .await?;

        self.save_devices(&mut transaction, &changes.devices.new)
            .await?;
        self.save_devices(&mut transaction, &changes.devices.changed)
            .await?;
        self.delete_devices(&mut transaction, &changes.devices.deleted)
            .await?;

        self.save_user_identities(&mut transaction, &changes.identities.new)
            .await?;
        self.save_user_identities(&mut transaction, &changes.identities.changed)
            .await?;
        self.save_olm_hashses(&mut transaction, &changes.message_hashes)
            .await?;

        transaction.commit().await?;

        Ok(())
    }

    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Arc<Mutex<Vec<Session>>>>> {
        let mut connection = self.connection.lock().await;
        Ok(self.get_sessions_for(&mut connection, sender_key).await?)
    }

    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>> {
        Ok(self
            .load_inbound_group_session_helper(room_id, sender_key, session_id)
            .await?)
    }

    async fn get_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>> {
        Ok(self.load_inbound_group_sessions().await?)
    }

    fn is_user_tracked(&self, user_id: &UserId) -> bool {
        self.tracked_users.contains(user_id)
    }

    fn has_users_for_key_query(&self) -> bool {
        !self.users_for_key_query.is_empty()
    }

    fn users_for_key_query(&self) -> HashSet<UserId> {
        #[allow(clippy::map_clone)]
        self.users_for_key_query.iter().map(|u| u.clone()).collect()
    }

    async fn update_tracked_user(&self, user: &UserId, dirty: bool) -> Result<bool> {
        let already_added = self.tracked_users.insert(user.clone());

        if dirty {
            self.users_for_key_query.insert(user.clone());
        } else {
            self.users_for_key_query.remove(user);
        }

        self.save_tracked_user(user, dirty).await?;

        Ok(already_added)
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<ReadOnlyDevice>> {
        self.get_single_device(user_id, device_id).await
    }

    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<DeviceIdBox, ReadOnlyDevice>> {
        Ok(self.load_devices(user_id).await?)
    }

    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<UserIdentities>> {
        self.load_user(user_id).await
    }

    async fn save_value(&self, key: String, value: String) -> Result<()> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        query("REPLACE INTO key_value (account_id, key, value) VALUES (?1, ?2, ?3)")
            .bind(account_id)
            .bind(&key)
            .bind(&value)
            .execute(&mut *connection)
            .await?;

        Ok(())
    }

    async fn remove_value(&self, key: &str) -> Result<()> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        query(
            "DELETE FROM key_value
             WHERE account_id = ?1 and key = ?2
             ",
        )
        .bind(account_id)
        .bind(key)
        .execute(&mut *connection)
        .await?;

        Ok(())
    }

    async fn get_value(&self, key: &str) -> Result<Option<String>> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let row: Option<(String,)> =
            query_as("SELECT value FROM key_value WHERE account_id = ? and key = ?")
                .bind(account_id)
                .bind(key)
                .fetch_optional(&mut *connection)
                .await?;

        Ok(row.map(|r| r.0))
    }

    async fn is_message_known(&self, message_hash: &OlmMessageHash) -> Result<bool> {
        let account_id = self.account_id().ok_or(CryptoStoreError::AccountUnset)?;
        let mut connection = self.connection.lock().await;

        let row: Option<(String,)> = query_as(
            "SELECT hash FROM olm_hashes WHERE account_id = ? and sender_key = ? and hash = ?",
        )
        .bind(account_id)
        .bind(&message_hash.sender_key)
        .bind(&message_hash.hash)
        .fetch_optional(&mut *connection)
        .await?;

        Ok(row.is_some())
    }
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> StdResult<(), std::fmt::Error> {
        fmt.debug_struct("SqliteStore")
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .field("path", &self.path)
            .finish()
    }
}

#[cfg(test)]
mod test {
    use crate::{
        identities::{
            device::test::get_device,
            user::test::{get_other_identity, get_own_identity},
        },
        olm::{
            GroupSessionKey, InboundGroupSession, OlmMessageHash, PrivateCrossSigningIdentity,
            ReadOnlyAccount, Session,
        },
        store::{Changes, DeviceChanges, IdentityChanges},
    };
    use matrix_sdk_common::{
        api::r0::keys::SignedKey,
        identifiers::{room_id, user_id, DeviceId, UserId},
    };
    use olm_rs::outbound_group_session::OlmOutboundGroupSession;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    use super::{CryptoStore, SqliteStore};

    fn alice_id() -> UserId {
        user_id!("@alice:example.org")
    }

    fn alice_device_id() -> Box<DeviceId> {
        "ALICEDEVICE".into()
    }

    fn bob_id() -> UserId {
        user_id!("@bob:example.org")
    }

    fn bob_device_id() -> Box<DeviceId> {
        "BOBDEVICE".into()
    }

    async fn get_store(passphrase: Option<&str>) -> (SqliteStore, tempfile::TempDir) {
        let tmpdir = tempdir().unwrap();
        let tmpdir_path = tmpdir.path().to_str().unwrap();

        let store = if let Some(passphrase) = passphrase {
            SqliteStore::open_with_passphrase(
                &alice_id(),
                &alice_device_id(),
                tmpdir_path,
                passphrase,
            )
            .await
            .expect("Can't create a passphrase protected store")
        } else {
            SqliteStore::open(&alice_id(), &alice_device_id(), tmpdir_path)
                .await
                .expect("Can't create store")
        };

        (store, tmpdir)
    }

    async fn get_loaded_store() -> (ReadOnlyAccount, SqliteStore, tempfile::TempDir) {
        let (store, dir) = get_store(None).await;
        let account = get_account();
        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        (account, store, dir)
    }

    fn get_account() -> ReadOnlyAccount {
        ReadOnlyAccount::new(&alice_id(), &alice_device_id())
    }

    async fn get_account_and_session() -> (ReadOnlyAccount, Session) {
        let alice = ReadOnlyAccount::new(&alice_id(), &alice_device_id());
        let bob = ReadOnlyAccount::new(&bob_id(), &bob_device_id());

        bob.generate_one_time_keys_helper(1).await;
        let one_time_key = bob
            .one_time_keys()
            .await
            .curve25519()
            .iter()
            .next()
            .unwrap()
            .1
            .to_owned();
        let one_time_key = SignedKey {
            key: one_time_key,
            signatures: BTreeMap::new(),
        };
        let sender_key = bob.identity_keys().curve25519().to_owned();
        let session = alice
            .create_outbound_session_helper(&sender_key, &one_time_key)
            .await
            .unwrap();

        (alice, session)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_store() {
        let tmpdir = tempdir().unwrap();
        let tmpdir_path = tmpdir.path().to_str().unwrap();
        let _ = SqliteStore::open(&alice_id(), &alice_device_id(), tmpdir_path)
            .await
            .expect("Can't create store");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn save_account() {
        let (store, _dir) = get_store(None).await;
        assert!(store.load_account().await.unwrap().is_none());
        let account = get_account();

        store
            .save_account(account)
            .await
            .expect("Can't save account");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn load_account() {
        let (store, _dir) = get_store(None).await;
        let account = get_account();

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        let loaded_account = store.load_account().await.expect("Can't load account");
        let loaded_account = loaded_account.unwrap();

        assert_eq!(account, loaded_account);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn load_account_with_passphrase() {
        let (store, _dir) = get_store(Some("secret_passphrase")).await;
        let account = get_account();

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        let loaded_account = store.load_account().await.expect("Can't load account");
        let loaded_account = loaded_account.unwrap();

        assert_eq!(account, loaded_account);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn save_and_share_account() {
        let (store, _dir) = get_store(None).await;
        let account = get_account();

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        account.mark_as_shared();
        account.update_uploaded_key_count(50);

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        let loaded_account = store.load_account().await.expect("Can't load account");
        let loaded_account = loaded_account.unwrap();

        assert_eq!(account, loaded_account);
        assert_eq!(
            account.uploaded_key_count(),
            loaded_account.uploaded_key_count()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn save_session() {
        let (store, _dir) = get_store(None).await;
        let (account, session) = get_account_and_session().await;

        assert!(store.save_sessions(&[session.clone()]).await.is_err());

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        store.save_sessions(&[session]).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn load_sessions() {
        let (store, _dir) = get_store(None).await;
        let (account, session) = get_account_and_session().await;
        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");
        store.save_sessions(&[session.clone()]).await.unwrap();

        let sessions = store
            .load_sessions_for(&session.sender_key)
            .await
            .expect("Can't load sessions");
        let loaded_session = &sessions[0];

        assert_eq!(&session, loaded_session);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_and_save_session() {
        let (store, dir) = get_store(None).await;
        let (account, session) = get_account_and_session().await;
        let sender_key = session.sender_key.to_owned();
        let session_id = session.session_id().to_owned();

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");
        store.save_sessions(&[session]).await.unwrap();

        let sessions = store.get_sessions(&sender_key).await.unwrap().unwrap();
        let sessions_lock = sessions.lock().await;
        let session = &sessions_lock[0];

        assert_eq!(session_id, session.session_id());

        drop(store);

        let store = SqliteStore::open(&alice_id(), &alice_device_id(), dir.path())
            .await
            .expect("Can't create store");

        let loaded_account = store.load_account().await.unwrap().unwrap();
        assert_eq!(account, loaded_account);

        let sessions = store.get_sessions(&sender_key).await.unwrap().unwrap();
        let sessions_lock = sessions.lock().await;
        let session = &sessions_lock[0];

        assert_eq!(session_id, session.session_id());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn save_inbound_group_session() {
        let (account, store, _dir) = get_loaded_store().await;

        let identity_keys = account.identity_keys();
        let outbound_session = OlmOutboundGroupSession::new();
        let session = InboundGroupSession::new(
            identity_keys.curve25519(),
            identity_keys.ed25519(),
            &room_id!("!test:localhost"),
            GroupSessionKey(outbound_session.session_key()),
        )
        .expect("Can't create session");

        store
            .save_inbound_group_sessions_test(&[session])
            .await
            .expect("Can't save group session");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn load_inbound_group_session() {
        let (account, store, dir) = get_loaded_store().await;

        let identity_keys = account.identity_keys();
        let outbound_session = OlmOutboundGroupSession::new();
        let session = InboundGroupSession::new(
            identity_keys.curve25519(),
            identity_keys.ed25519(),
            &room_id!("!test:localhost"),
            GroupSessionKey(outbound_session.session_key()),
        )
        .expect("Can't create session");

        let mut export = session.export().await;

        export.forwarding_curve25519_key_chain = vec!["some_chain".to_owned()];

        let session = InboundGroupSession::from_export(export).unwrap();

        store
            .save_inbound_group_sessions_test(&[session.clone()])
            .await
            .expect("Can't save group session");

        let store = SqliteStore::open(&alice_id(), &alice_device_id(), dir.path())
            .await
            .expect("Can't create store");

        store.load_account().await.unwrap();

        let loaded_session = store
            .get_inbound_group_session(&session.room_id, &session.sender_key, session.session_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session, loaded_session);
        let export = loaded_session.export().await;
        assert!(!export.forwarding_curve25519_key_chain.is_empty())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_tracked_users() {
        let (_account, store, dir) = get_loaded_store().await;
        let device = get_device();

        assert!(store
            .update_tracked_user(device.user_id(), false)
            .await
            .unwrap());
        assert!(!store
            .update_tracked_user(device.user_id(), false)
            .await
            .unwrap());

        assert!(store.is_user_tracked(device.user_id()));
        assert!(!store.users_for_key_query().contains(device.user_id()));
        assert!(!store
            .update_tracked_user(device.user_id(), true)
            .await
            .unwrap());
        assert!(store.users_for_key_query().contains(device.user_id()));
        drop(store);

        let store = SqliteStore::open(&alice_id(), &alice_device_id(), dir.path())
            .await
            .expect("Can't create store");

        store.load_account().await.unwrap();

        assert!(store.is_user_tracked(device.user_id()));
        assert!(store.users_for_key_query().contains(device.user_id()));

        store
            .update_tracked_user(device.user_id(), false)
            .await
            .unwrap();
        assert!(!store.users_for_key_query().contains(device.user_id()));

        let store = SqliteStore::open(&alice_id(), &alice_device_id(), dir.path())
            .await
            .expect("Can't create store");

        store.load_account().await.unwrap();

        assert!(!store.users_for_key_query().contains(device.user_id()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn device_saving() {
        let (_account, store, dir) = get_loaded_store().await;
        let device = get_device();

        let changes = Changes {
            devices: DeviceChanges {
                changed: vec![device.clone()],
                ..Default::default()
            },
            ..Default::default()
        };

        store.save_changes(changes).await.unwrap();

        drop(store);

        let store = SqliteStore::open(&alice_id(), &alice_device_id(), dir.path())
            .await
            .expect("Can't create store");

        store.load_account().await.unwrap();

        let loaded_device = store
            .get_device(device.user_id(), device.device_id())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(device, loaded_device);

        for algorithm in loaded_device.algorithms() {
            assert!(device.algorithms().contains(algorithm));
        }
        assert_eq!(device.algorithms().len(), loaded_device.algorithms().len());
        assert_eq!(device.keys(), loaded_device.keys());

        let user_devices = store.get_user_devices(device.user_id()).await.unwrap();
        assert_eq!(&**user_devices.keys().next().unwrap(), device.device_id());
        assert_eq!(user_devices.values().next().unwrap(), &device);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn device_deleting() {
        let (_account, store, dir) = get_loaded_store().await;
        let device = get_device();

        let changes = Changes {
            devices: DeviceChanges {
                changed: vec![device.clone()],
                ..Default::default()
            },
            ..Default::default()
        };

        store.save_changes(changes).await.unwrap();

        let changes = Changes {
            devices: DeviceChanges {
                deleted: vec![device.clone()],
                ..Default::default()
            },
            ..Default::default()
        };

        store.save_changes(changes).await.unwrap();

        let store = SqliteStore::open(&alice_id(), &alice_device_id(), dir.path())
            .await
            .expect("Can't create store");

        store.load_account().await.unwrap();

        let loaded_device = store
            .get_device(device.user_id(), device.device_id())
            .await
            .unwrap();

        assert!(loaded_device.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn user_saving() {
        let dir = tempdir().unwrap();
        let tmpdir_path = dir.path().to_str().unwrap();

        let user_id = user_id!("@example:localhost");
        let device_id: &DeviceId = "WSKKLTJZCL".into();

        let store = SqliteStore::open(&user_id, &device_id, tmpdir_path)
            .await
            .expect("Can't create store");

        let account = ReadOnlyAccount::new(&user_id, &device_id);

        store
            .save_account(account.clone())
            .await
            .expect("Can't save account");

        let own_identity = get_own_identity();

        let changes = Changes {
            identities: IdentityChanges {
                changed: vec![own_identity.clone().into()],
                ..Default::default()
            },
            ..Default::default()
        };

        store
            .save_changes(changes)
            .await
            .expect("Can't save identity");

        drop(store);

        let store = SqliteStore::open(&user_id, &device_id, dir.path())
            .await
            .expect("Can't create store");

        store.load_account().await.unwrap();

        let loaded_user = store
            .get_user_identity(own_identity.user_id())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(loaded_user.master_key(), own_identity.master_key());
        assert_eq!(
            loaded_user.self_signing_key(),
            own_identity.self_signing_key()
        );
        assert_eq!(loaded_user, own_identity.clone().into());

        let other_identity = get_other_identity();

        let changes = Changes {
            identities: IdentityChanges {
                changed: vec![other_identity.clone().into()],
                ..Default::default()
            },
            ..Default::default()
        };

        store.save_changes(changes).await.unwrap();

        let loaded_user = store
            .load_user(other_identity.user_id())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(loaded_user.master_key(), other_identity.master_key());
        assert_eq!(
            loaded_user.self_signing_key(),
            other_identity.self_signing_key()
        );
        assert_eq!(loaded_user, other_identity.into());

        own_identity.mark_as_verified();

        let changes = Changes {
            identities: IdentityChanges {
                changed: vec![own_identity.into()],
                ..Default::default()
            },
            ..Default::default()
        };

        store.save_changes(changes).await.unwrap();
        let loaded_user = store.load_user(&user_id).await.unwrap().unwrap();
        assert!(loaded_user.own().unwrap().is_verified())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn private_identity_saving() {
        let (_, store, _dir) = get_loaded_store().await;
        assert!(store.load_identity().await.unwrap().is_none());
        let identity = PrivateCrossSigningIdentity::new((&*store.user_id).clone()).await;

        let changes = Changes {
            private_identity: Some(identity.clone()),
            ..Default::default()
        };

        store.save_changes(changes).await.unwrap();
        let loaded_identity = store.load_identity().await.unwrap().unwrap();
        assert_eq!(identity.user_id(), loaded_identity.user_id());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn key_value_saving() {
        let (_, store, _dir) = get_loaded_store().await;
        let key = "test_key".to_string();
        let value = "secret value".to_string();

        store.save_value(key.clone(), value.clone()).await.unwrap();
        let stored_value = store.get_value(&key).await.unwrap().unwrap();

        assert_eq!(value, stored_value);

        store.remove_value(&key).await.unwrap();
        assert!(store.get_value(&key).await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn olm_hash_saving() {
        let (_, store, _dir) = get_loaded_store().await;

        let hash = OlmMessageHash {
            sender_key: "test_sender".to_owned(),
            hash: "test_hash".to_owned(),
        };

        let mut changes = Changes::default();
        changes.message_hashes.push(hash.clone());

        assert!(!store.is_message_known(&hash).await.unwrap());
        store.save_changes(changes).await.unwrap();
        assert!(store.is_message_known(&hash).await.unwrap());
    }
}
