// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
use std::{
    fs::File,
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
};

use diesel::connection::SimpleConnection;
use olpc_cjson::CanonicalFormatter;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use typed_builder::TypedBuilder;

use crate::{
    db::{models as m, ConnectionPool, DeferredTxConnection, ExclusiveTxConnection},
    device::{DeviceIdentifier, OperatingSystem},
    encryption::{
        BackupPassword, DataEncryptionKey, EncryptionOutput, KdfNonce, KdfSecret,
        KeyEncryptionKey, KeyName, Keychain, RootBackupKey,
    },
    utils::unix_timestamp,
    Error,
};

// File names inside the backup zip
const ENCRYPTED_BACKUP_FILE_NAME: &str = "backup.sqlite3.encrypted";
const METADATA_FILE_NAME: &str = "metadata.json";

/// The backup scheme version.
#[derive(
    Copy,
    Clone,
    Debug,
    Hash,
    PartialEq,
    Eq,
    strum_macros::AsRefStr,
    strum_macros::Display,
    strum_macros::EnumString,
    strum_macros::EnumIter,
    Serialize,
    Deserialize,
)]
#[strum(serialize_all = "lowercase")]
pub enum BackupVersion {
    V1,
}

/// Saved as a plaintext json file along with the encrypted backup.
/// More info: https://sealvault.org/dev-docs/design/backup/#backup-contents
#[derive(Debug, PartialEq, Serialize, Deserialize, TypedBuilder)]
pub struct BackupMetadata {
    /// The backup implementation version
    backup_scheme_version: BackupVersion,
    /// The backup version from the database. Monotonically increasing integer within a device.
    backup_version: i64,
    /// Unix timestamp
    #[builder(default_code = "unix_timestamp()")]
    timestamp: i64,
    device_identifier: DeviceIdentifier,
    #[builder(default)]
    operating_system: OperatingSystem,
    /// Base-64 encoded KDF nonce
    #[builder(setter(into))]
    kdf_nonce: String,
}

impl BackupMetadata {
    fn backup_file_name(&self) -> PathBuf {
        format!(
            "sealvault_backup_{}_{}_{}_{}_{}.zip",
            self.backup_scheme_version,
            self.operating_system,
            self.timestamp,
            self.device_identifier,
            self.backup_version
        )
        .into()
    }

    /// Use this for a canonical serialization of the backup metadata to make sure that the
    /// associated data in the AEAD matches.
    pub fn canonical_json(&self) -> Result<Vec<u8>, Error> {
        let mut buf = Vec::new();
        let mut ser =
            serde_json::Serializer::with_formatter(&mut buf, CanonicalFormatter::new());
        self.serialize(&mut ser).map_err(|_| Error::Fatal {
            error: "Failed to serialize backup metadata.".into(),
        })?;
        Ok(buf)
    }
}

/// Set up backup keys for the user and return the backup password that can be displayed to the
/// user. Calling the function again will rotate the password and associated keys.
pub fn set_up_or_rotate_backup(
    connection_pool: &ConnectionPool,
    keychain: &Keychain,
    device_identifier: &DeviceIdentifier,
) -> Result<String, Error> {
    // In an exclusive transaction to prevent running in parallel and potentially ending up with
    // mixed up keychain items.
    let res = connection_pool.exclusive_transaction(|mut tx_conn| {
        let (kdf_nonce, backup_password) =
            setup_or_rotate_keys_for_backup(keychain, device_identifier, &mut tx_conn)?;

        enable_backups_in_db(&mut tx_conn, &kdf_nonce)?;

        Ok(backup_password.display_to_user())
    });

    // Rollback on error
    if res.is_err() {
        rollback_setup_backup(connection_pool, keychain, device_identifier)?;
    }

    res
}

fn setup_or_rotate_keys_for_backup(
    keychain: &Keychain,
    device_identifier: &DeviceIdentifier,
    tx_conn: &mut ExclusiveTxConnection,
) -> Result<(KdfNonce, BackupPassword), Error> {
    // Create new backup password
    // KDF secret should be first to abort if something's wrong with keychain sync which is the
    // most likely failure scenario.
    let kdf_secret = KdfSecret::setup_or_rotate(keychain, device_identifier)?;
    let backup_password = BackupPassword::setup_or_rotate(keychain)?;
    let kdf_nonce: KdfNonce = KdfNonce::random()?;

    let root_backup_key =
        RootBackupKey::derive_from(&backup_password, &kdf_secret, &kdf_nonce)?;

    let db_backup_dek = root_backup_key.derive_db_backup_dek()?;
    db_backup_dek.upsert_to_local_keychain(keychain)?;

    let sk_backup_kek = root_backup_key.derive_sk_backup_kek()?;
    sk_backup_kek.upsert_to_local_keychain(keychain)?;
    let sk_backup_kek = KeyEncryptionKey::sk_backup_kek(keychain)?;

    let sk_kek = KeyEncryptionKey::sk_kek(keychain)?;

    // Fetch secret key data encryption key (SK-DEK) from DB and decrypt it with secret key
    // encryption key (SK-KEK) from local keychain.
    let (sk_dek_id, sk_dek) = m::DataEncryptionKey::fetch_dek(
        tx_conn.as_mut(),
        KeyName::SkDataEncryptionKey,
        &sk_kek,
    )?;

    // Encrypt SK-DEK with the secret key backup encryption key and save the encrypted SK-DEK in
    // the database.
    let encrypted_sk_dek = sk_dek.to_encrypted(&sk_backup_kek)?;
    let maybe_sk_dek_backup_id = m::LocalEncryptedDek::fetch_id(
        tx_conn.as_mut(),
        &sk_dek_id,
        sk_backup_kek.name(),
    )?;
    if let Some(sk_dek_backup_id) = maybe_sk_dek_backup_id {
        m::LocalEncryptedDek::set_encrypted_dek(
            tx_conn.as_mut(),
            sk_dek_backup_id,
            &encrypted_sk_dek,
        )?;
    } else {
        m::NewLocalEncryptedDek::builder()
            .dek_id(&sk_dek_id)
            .encrypted_dek(&encrypted_sk_dek)
            .kek_name(sk_backup_kek.name())
            .build()
            .insert(tx_conn.as_mut())?;
    }

    Ok((kdf_nonce, backup_password))
}

fn enable_backups_in_db(
    tx_conn: &mut ExclusiveTxConnection,
    kdf_nonce: &KdfNonce,
) -> Result<(), Error> {
    // Update timestamps
    m::LocalSettings::set_backup_kdf_nonce(tx_conn.as_mut(), Some(kdf_nonce))?;
    m::LocalSettings::update_backup_password_timestamp(tx_conn.as_mut())?;
    // Reset user viewed password
    m::LocalSettings::set_backup_enabled(tx_conn.as_mut(), true)?;
    Ok(())
}

fn rollback_setup_backup(
    connection_pool: &ConnectionPool,
    keychain: &Keychain,
    device_identifier: &DeviceIdentifier,
) -> Result<(), Error> {
    // First make sure we disable backups as keychain might be in inconsistent state.
    // Rolling back the changes from the transaction is not enough, because backups might have
    // been enabled prior.
    connection_pool.deferred_transaction(disable_backups_in_db)?;
    // Try to clean up keychain items. The application works correctly even if they're not
    // all successfully cleaned up.
    remove_keys_for_backup(keychain, device_identifier);
    Ok(())
}

fn remove_keys_for_backup(keychain: &Keychain, device_identifier: &DeviceIdentifier) {
    let _ = KdfSecret::delete_from_keychain_if_exists(keychain, device_identifier);
    let _ = BackupPassword::delete_from_keychain_if_exists(keychain);
    let _ = KeyEncryptionKey::delete_from_keychain_if_exists(
        keychain,
        KeyName::SkBackupKeyEncryptionKey,
    );
    let _ = DataEncryptionKey::delete_from_keychain_if_exists(
        keychain,
        KeyName::DbBackupDataEncryptionKey,
    );
}

fn disable_backups_in_db(mut tx_conn: DeferredTxConnection) -> Result<(), Error> {
    m::LocalSettings::set_backup_kdf_nonce(tx_conn.as_mut(), None)?;
    m::LocalSettings::set_backup_enabled(tx_conn.as_mut(), false)?;
    Ok(())
}

/// Create backup to the desired directory if needed. The directory is assumed to exist.
/// Returns the backup metadata if a backup was created.
/// A backup is needed if the pending backup version matches the completed backup version in the
/// database.
/// The backup is a zip file that contains an encrypted database backup and the metadata. Returns
/// the path to the zip file. More info:
/// https://sealvault.org/dev-docs/design/backup/#backup-contents
pub fn create_backup(
    connection_pool: &ConnectionPool,
    keychain: &Keychain,
    device_identifier: DeviceIdentifier,
    out_dir: &Path,
) -> Result<Option<BackupMetadata>, Error> {
    if let Some(metadata) =
        db_backup(connection_pool, keychain, device_identifier, out_dir)?
    {
        // TODO clean up old versions
        Ok(Some(metadata))
    } else {
        Ok(None)
    }
}

pub fn restore_backup(
    keychain: &Keychain,
    db_backup_dek: &DataEncryptionKey,
    sk_backup_kek: &KeyEncryptionKey,
    device_id: &DeviceIdentifier,
    from_zip: &Path,
    to_path: &Path,
) -> Result<BackupMetadata, Error> {
    let backup_metadata_bytes =
        extract_from_zip(from_zip, METADATA_FILE_NAME).map_err(map_zip_error)?;
    let metadata: BackupMetadata = serde_json::from_slice(&backup_metadata_bytes)
        .map_err(|_err| Error::Retriable {
            error: "Failed to deserialize backup metadata".into(),
        })?;

    let encrypted_backup_bytes =
        extract_from_zip(from_zip, ENCRYPTED_BACKUP_FILE_NAME).map_err(map_zip_error)?;
    let encryption_output: EncryptionOutput = encrypted_backup_bytes.try_into()?;

    let decrypted_backup = db_backup_dek.decrypt_backup(&encryption_output, &metadata)?;
    restore_decrypted_backup(&metadata, &decrypted_backup, to_path)?;

    let backup_connection_pool =
        set_up_or_rotate_sk_kek(keychain, sk_backup_kek, to_path)?;

    // Rotate backup password and associated secrets
    set_up_or_rotate_backup(&backup_connection_pool, keychain, device_id)?;

    Ok(metadata)
}

fn restore_decrypted_backup(
    metadata: &BackupMetadata,
    decrypted_backup: &[u8],
    to_path: &Path,
) -> Result<(), Error> {
    let mut restored_file = File::create(to_path).map_err(|err| Error::Retriable {
        error: format!("Failed to create restored backup file with error: '{err}'"),
    })?;
    restored_file
        .write_all(decrypted_backup)
        .map_err(|err| Error::Retriable {
            error: format!("Failed to write to restored backup file with error: '{err}'"),
        })?;
    verify_backup(to_path, metadata.backup_version)?;
    Ok(())
}

/// Set up SK-KEK if we're on new device as it's not on the local keychain. Rotate if the app is
/// reinstalled on same device for hygiene.
/// It's possible that SK-KEK already exists when reinstalling on same device since iOS Keychain
/// items are not deleted when the app is deleted.
fn set_up_or_rotate_sk_kek(
    keychain: &Keychain,
    sk_backup_kek: &KeyEncryptionKey,
    db_path: &Path,
) -> Result<ConnectionPool, Error> {
    let backup_cp = backup_connection_pool(db_path)?;
    backup_cp.exclusive_transaction(|mut tx_conn| {
        // Fetch the secret key data encryption key (SK-DEK) from the database and decrypt it with the backup key encryption key
        let (dek_id, sk_dek) = m::DataEncryptionKey::fetch_dek(tx_conn.as_mut(), KeyName::SkDataEncryptionKey, sk_backup_kek)?;
        // Create a new secret key encryption key (SK-KEK)
        let sk_kek = KeyEncryptionKey::random(KeyName::SkKeyEncryptionKey)?;
        // Encrypt the SK-DEK with the new SK-KEK
        let encrypted_sk_dek = sk_dek.to_encrypted(&sk_kek)?;
        // Update the encrypted SK-DEK in the db to the one newly encrypted with new SK-KEK.
        // The local encrypted SK-DEK with SK-KEK from previous device is assumed to exist in the
        // backup, since it's created in the first data migration, ergo all backups should have it.
        let sk_dek_id = m::LocalEncryptedDek::fetch_id(tx_conn.as_mut(), &dek_id, sk_kek.name())?.ok_or_else(|| Error::Fatal {
            error: "Local encrypted SK-DEK with SK-KEK is assumed to exist when calling `rotate_sk_kek`".into()
        })?;
        m::LocalEncryptedDek::set_encrypted_dek(tx_conn.as_mut(), sk_dek_id, &encrypted_sk_dek)?;
        // Save the new SK-KEK to the keychain.
        sk_kek.upsert_to_local_keychain(keychain)
    })?;
    Ok(backup_cp)
}

fn db_backup(
    connection_pool: &ConnectionPool,
    keychain: &Keychain,
    device_identifier: DeviceIdentifier,
    out_dir: &Path,
) -> Result<Option<BackupMetadata>, Error> {
    // We check this twice to avoid running the expensive wal checkpointing if a backup is not
    // needed.
    let pending = connection_pool
        .exclusive_transaction(|mut tx_conn| pending_backup_version(&mut tx_conn))?;
    if pending.is_none() {
        return Ok(None);
    }
    // Flush WAL to the DB file. Can't be inside exclusive transaction, because it acquires its own
    // lock.
    let mut conn = connection_pool.connection()?;
    conn.batch_execute("PRAGMA wal_checkpoint(FULL);")?;

    connection_pool.exclusive_transaction(|mut tx_conn| {
        if let Some(backup_version) = pending_backup_version(&mut tx_conn)? {
            // Fetch these while holding the exclusive lock to make sure they match
            // the secret key backup encryption key that was used to encrypt the secret key data
            // encryption key which is stored inside the DB.
            let kdf_nonce =
                m::LocalSettings::fetch_kdf_nonce(tx_conn.as_mut())?.ok_or_else(|| {
                    Error::Fatal {
                        error: "No KDF nonce in DB. Make sure backup is set up before attempting \
                        to create one.".into()
                    }
                })?;
            let db_backup_dek = DataEncryptionKey::db_backup_dek(keychain)?;

            let backup_contents =
                create_verified_backup(connection_pool.db_path(), backup_version)?;

            let metadata = BackupMetadata::builder()
                .backup_scheme_version(BackupVersion::V1)
                .backup_version(backup_version)
                .device_identifier(device_identifier)
                .kdf_nonce(&kdf_nonce)
                .build();

            let encryption_output =
                db_backup_dek.encrypt_backup(&backup_contents, &metadata)?;

            create_backup_zip(out_dir, &metadata, &encryption_output)?;

            m::LocalSettings::set_completed_backup_version(tx_conn.as_mut(), backup_version)?;

            Ok(Some(metadata))
        } else {
            Ok(None)
        }
    })
}

/// Returns the pending backup version if backups are enabled and we need a backup.
/// We need a backup if we haven't backed up all keys yet.
fn pending_backup_version(
    tx_conn: &mut ExclusiveTxConnection,
) -> Result<Option<i64>, Error> {
    let backup_enabled = m::LocalSettings::fetch_backup_enabled(tx_conn.as_mut())?;
    if !backup_enabled {
        return Ok(None);
    }
    let num_keys = m::AsymmetricKey::num_keys(tx_conn.as_mut())?;
    let completed_backup_version =
        m::LocalSettings::fetch_completed_backup_version(tx_conn.as_mut())?;
    if num_keys > completed_backup_version {
        Ok(Some(num_keys))
    } else {
        Ok(None)
    }
}

/// Create a verified backup of the DB and return it as bytes.
fn create_verified_backup(db_path: &Path, backup_version: i64) -> Result<Vec<u8>, Error> {
    let mut db_file = File::open(db_path).map_err(|err| Error::Retriable {
        error: format!("Failed to open DB file: {err}"),
    })?;

    let mut backup_file = NamedTempFile::new().map_err(|err| Error::Retriable {
        error: format!("Failed to create temporary file with error: '{err}'"),
    })?;

    // Sqlite C backup api would be preferable to copying, but it's not supported by Diesel.
    // Copy while holding lock to make sure DB doesn't change.
    std::io::copy(&mut db_file, &mut backup_file).map_err(|err| Error::Retriable {
        error: format!("Failed to copy DB file to backup file: {err}"),
    })?;

    verify_backup(backup_file.path(), backup_version)?;

    let mut backup_contents: Vec<u8> = Vec::new();
    backup_file.rewind().map_err(|err| Error::Retriable {
        error: format!("Failed to move cursor to start of file with error: '{err}'"),
    })?;
    backup_file
        .read_to_end(&mut backup_contents)
        .map_err(|err| Error::Retriable {
            error: format!("Failed to read backup file contents with error: '{err}'"),
        })?;

    Ok(backup_contents)
}

fn create_backup_zip(
    out_dir: &Path,
    metadata: &BackupMetadata,
    encryption_output: &EncryptionOutput,
) -> Result<(), Error> {
    let out_path = out_dir.join(metadata.backup_file_name());
    // Overwrites existing file which is important to rerun if setting the completed backup
    // version fails.
    let out_file = File::create(out_path).map_err(|err| Error::Retriable {
        error: format!("Failed to create backup archive file with error: '{err}'"),
    })?;

    let meta_ser = metadata.canonical_json()?;

    create_backup_zip_inner(out_file, encryption_output, &meta_ser)
        .map_err(map_zip_error)?;

    Ok(())
}

fn map_zip_error(err: zip::result::ZipError) -> Error {
    Error::Retriable {
        error: format!("Failed to create backup zip with error: '{err}'"),
    }
}

fn create_backup_zip_inner(
    out_file: File,
    encryption_output: &EncryptionOutput,
    metadata_serialized: &[u8],
) -> Result<(), zip::result::ZipError> {
    use zip::{write::FileOptions, CompressionMethod, ZipWriter};

    let mut zip_file = ZipWriter::new(out_file);
    // No compression as the backup is encrypted which doesn't compress much.
    let zip_options =
        FileOptions::default().compression_method(CompressionMethod::Stored);

    zip_file.start_file(ENCRYPTED_BACKUP_FILE_NAME, zip_options)?;
    // Includes the nonce
    let encrypted_bytes: Vec<u8> = encryption_output.into();
    zip_file.write_all(&encrypted_bytes)?;

    let zip_options =
        FileOptions::default().compression_method(CompressionMethod::Stored);
    zip_file.start_file(METADATA_FILE_NAME, zip_options)?;
    zip_file.write_all(metadata_serialized)?;

    zip_file.finish()?;

    Ok(())
}

fn extract_from_zip(
    path: &Path,
    file_name: &str,
) -> Result<Vec<u8>, zip::result::ZipError> {
    let file = File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    let mut file_bytes: Vec<u8> = Default::default();
    let mut backup_file = archive.by_name(file_name)?;
    backup_file.read_to_end(&mut file_bytes)?;

    Ok(file_bytes)
}

fn backup_connection_pool(backup_path: &Path) -> Result<ConnectionPool, Error> {
    let backup_path = backup_path.to_str().ok_or_else(|| Error::Fatal {
        error: "Failed to convert backup path to utf-8".into(),
    })?;
    ConnectionPool::new(backup_path)
}

fn verify_backup(backup_path: &Path, expected_backup_version: i64) -> Result<(), Error> {
    let backup_cp = backup_connection_pool(backup_path)?;

    let maybe_version = backup_cp
        .exclusive_transaction(|mut tx_conn| pending_backup_version(&mut tx_conn))?;

    if let Some(version) = maybe_version {
        if version == expected_backup_version {
            Ok(())
        } else {
            Err(Error::Fatal {
                error: format!(
                    "Expected pending backup version to be {expected_backup_version} in \
                            backup, instead it is: {version}"
                ),
            })
        }
    } else {
        Err(Error::Fatal {
            error: "Backups are not enabled in restored backup".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use anyhow::Result;

    use super::*;
    use crate::{
        db::{data_migrations, schema_migrations::run_migrations},
        encryption::Keychain,
        protocols::eth,
        public_suffix_list::PublicSuffixList,
    };

    struct BackupTest {
        device_id: DeviceIdentifier,
        tmp_dir: tempfile::TempDir,
        backup_dir: PathBuf,
        db_path: PathBuf,
        keychain: Keychain,
        psl: PublicSuffixList,
        connection_pool: ConnectionPool,
    }

    impl BackupTest {
        fn new() -> Result<Self> {
            let device_id: DeviceIdentifier = "test-device".to_string().try_into()?;
            let tmp_dir = tempfile::tempdir().expect("creates temp dir");
            let backup_dir = tmp_dir.path().join("backups");
            fs::create_dir(backup_dir.as_path())?;
            let db_path = tmp_dir.path().join("db.sqlite3");

            // Create mock db
            let connection_pool =
                ConnectionPool::new(db_path.to_str().expect("ok utf-8"))?;
            let keychain = Keychain::new();
            let psl = PublicSuffixList::new()?;
            connection_pool.exclusive_transaction(|mut tx_conn| {
                run_migrations(&mut tx_conn)?;
                data_migrations::run_all(tx_conn, &keychain, &psl)
            })?;

            Ok(Self {
                device_id,
                tmp_dir,
                backup_dir,
                db_path,
                keychain,
                psl,
                connection_pool,
            })
        }

        fn setup_or_rotate_backup(&self) -> Result<String> {
            let pwd = set_up_or_rotate_backup(
                &self.connection_pool,
                &self.keychain,
                &self.device_id,
            )?;
            Ok(pwd)
        }

        fn create_backup(&self) -> Result<Option<BackupMetadata>> {
            let metadata = create_backup(
                &self.connection_pool,
                &self.keychain,
                self.device_id.clone(),
                self.backup_dir.as_path(),
            )?;
            Ok(metadata)
        }

        fn trigger_need_new_backup(&self) -> Result<()> {
            self.connection_pool.deferred_transaction(|mut tx_conn| {
                let account_params = m::AccountParams::builder()
                    .name("Backup")
                    .bundled_picture_name("cat-tabby")
                    .build();
                // Creates a new account with a new wallet key.
                // A new backup is needed after a key is created.
                let _ = m::Account::create_eth_account(
                    &mut tx_conn,
                    &self.keychain,
                    &account_params,
                )?;
                Ok(())
            })?;
            Ok(())
        }
    }

    struct RestoreTest<'a> {
        backup: &'a BackupTest,
        keychain: Keychain,
        restore_to: NamedTempFile,
    }

    impl<'a> RestoreTest<'a> {
        fn new(backup: &'a BackupTest) -> Result<Self> {
            // Create new keychain to simulate running on other device but copy over kdf secret
            // which is synced.
            let kdf_secret =
                KdfSecret::from_keychain(&backup.keychain, &backup.device_id)?;
            let keychain = Keychain::new();
            kdf_secret.save_to_keychain(&keychain, &backup.device_id)?;
            Ok(Self {
                keychain,
                backup,
                restore_to: NamedTempFile::new()?,
            })
        }

        fn restore(
            &self,
            password: &str,
            metadata: &BackupMetadata,
        ) -> Result<BackupMetadata> {
            let password: BackupPassword = password.parse()?;
            let kdf_secret =
                KdfSecret::from_keychain(&self.keychain, &metadata.device_identifier)?;
            let kdf_nonce: KdfNonce = metadata.kdf_nonce.parse()?;
            let root_backup_key =
                RootBackupKey::derive_from(&password, &kdf_secret, &kdf_nonce)?;
            let db_backup_dek = root_backup_key.derive_db_backup_dek()?;
            let sk_backup_kek = root_backup_key.derive_sk_backup_kek()?;

            let restore_from = self.backup.backup_dir.join(metadata.backup_file_name());
            let backup_metadata = restore_backup(
                &self.keychain,
                &db_backup_dek,
                &sk_backup_kek,
                &metadata.device_identifier,
                restore_from.as_path(),
                self.restore_to.path(),
            )?;

            Ok(backup_metadata)
        }

        fn verify_can_decrypt_key(&self) -> Result<()> {
            let connection_pool = ConnectionPool::new(
                self.restore_to
                    .path()
                    .to_str()
                    .expect("path converts to str"),
            )?;
            let _ = connection_pool.deferred_transaction(|mut tx_conn| {
                let accounts = m::Account::list_all(tx_conn.as_mut())?;
                let account = accounts.first().expect("there is an account");
                let address_id = m::Address::fetch_eth_wallet_id(
                    &mut tx_conn,
                    &account.deterministic_id,
                    eth::ChainId::default_wallet_chain(),
                )?;
                // Uses SK-KEK to decrypt
                m::Address::fetch_eth_signing_key(
                    &mut tx_conn,
                    &self.keychain,
                    &address_id,
                )
            })?;
            Ok(())
        }

        fn verify(&self, password: &str, backup_metadata: &BackupMetadata) -> Result<()> {
            let restore_metadata = self.restore(&password, backup_metadata)?;
            self.verify_can_decrypt_key()?;
            assert_eq!(backup_metadata, &restore_metadata);
            Ok(())
        }
    }

    #[test]
    fn can_restore_db_backup() -> Result<()> {
        let backup = BackupTest::new()?;
        let password = backup.setup_or_rotate_backup()?;
        let backup_metadata = backup.create_backup()?.expect("needs backup");

        let restore = RestoreTest::new(&backup)?;
        restore.verify(&password, &backup_metadata)?;

        Ok(())
    }

    #[test]
    fn can_restore_after_multiple_backup() -> Result<()> {
        let backup = BackupTest::new()?;
        let password = backup.setup_or_rotate_backup()?;

        // First backup
        let backup_metadata = backup.create_backup()?.expect("needs backup");
        let initial_backup_version = backup_metadata.backup_version;
        assert!(initial_backup_version > 0);

        // No backup if no change.
        assert_eq!(backup.create_backup()?, None);

        backup.trigger_need_new_backup()?;
        let backup_metadata = backup.create_backup()?.expect("needs backup");
        assert!(initial_backup_version < backup_metadata.backup_version);

        let restore = RestoreTest::new(&backup)?;
        restore.verify(&password, &backup_metadata)?;

        Ok(())
    }

    #[test]
    fn can_rotate_password() -> Result<()> {
        let backup = BackupTest::new()?;
        let _ = backup.setup_or_rotate_backup()?;
        let password = backup.setup_or_rotate_backup()?;
        let backup_metadata = backup.create_backup()?.expect("needs backup");

        let restore = RestoreTest::new(&backup)?;
        restore.verify(&password, &backup_metadata)?;

        Ok(())
    }

    #[test]
    fn can_rollback_setup() -> Result<()> {
        let backup = BackupTest::new()?;

        let _ = backup.setup_or_rotate_backup()?;
        rollback_setup_backup(
            &backup.connection_pool,
            &backup.keychain,
            &backup.device_id,
        )?;
        // Shouldn't create backup after rollback
        assert_eq!(backup.create_backup()?, None);

        let password = backup.setup_or_rotate_backup()?;
        let backup_metadata = backup.create_backup()?.expect("needs backup");

        let restore = RestoreTest::new(&backup)?;
        restore.verify(&password, &backup_metadata)?;

        Ok(())
    }

    #[test]
    fn can_rollback_rotate() -> Result<()> {
        let backup = BackupTest::new()?;

        let _ = backup.setup_or_rotate_backup()?;
        let _ = backup.setup_or_rotate_backup()?;
        rollback_setup_backup(
            &backup.connection_pool,
            &backup.keychain,
            &backup.device_id,
        )?;
        assert_eq!(backup.create_backup()?, None);
        let password = backup.setup_or_rotate_backup()?;

        let backup_metadata = backup.create_backup()?.expect("needs backup");

        let restore = RestoreTest::new(&backup)?;
        restore.verify(&password, &backup_metadata)?;

        Ok(())
    }
}
