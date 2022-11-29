// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use diesel::prelude::*;

use crate::{
    db::schema::local_settings, encryption::KdfNonce, utils::rfc3339_timestamp, Error,
};

#[derive(Debug, PartialEq, Eq, Queryable, Identifiable)]
#[diesel(table_name = local_settings)]
pub struct LocalSettings {
    pub id: String,
    pub account_id: String,
    pub backup_enabled: bool,
    pub completed_backup_version: i64,
    pub backup_completed_at: Option<String>,
    pub backup_password_updated_at: Option<String>,
    pub backup_kdf_nonce: Option<Vec<u8>>,
}

const SINGLETON_ID: &str = "local_settings";

impl LocalSettings {
    pub fn create(
        connection: &mut SqliteConnection,
        account_id: &str,
    ) -> Result<(), Error> {
        use local_settings::dsl as ls;

        diesel::insert_into(local_settings::table)
            .values((ls::id.eq(SINGLETON_ID), ls::account_id.eq(account_id)))
            .execute(connection)?;

        Ok(())
    }

    pub fn fetch_active_account_id(
        connection: &mut SqliteConnection,
    ) -> Result<String, Error> {
        use local_settings::dsl as ls;

        let account_id = local_settings::table
            .find(&SINGLETON_ID)
            .select(ls::account_id)
            .first(connection)?;
        Ok(account_id)
    }

    pub fn set_completed_backup_version(
        connection: &mut SqliteConnection,
        completed_backup_version: i64,
    ) -> Result<(), Error> {
        use local_settings::dsl as ls;

        diesel::update(local_settings::table.find(&SINGLETON_ID))
            .set(ls::completed_backup_version.eq(completed_backup_version))
            .execute(connection)?;

        Ok(())
    }

    pub fn fetch_completed_backup_version(
        connection: &mut SqliteConnection,
    ) -> Result<i64, Error> {
        use local_settings::dsl as ls;

        let pending_backup_version = local_settings::table
            .find(&SINGLETON_ID)
            .select(ls::completed_backup_version)
            .first(connection)?;

        Ok(pending_backup_version)
    }

    pub fn fetch_backup_enabled(
        connection: &mut SqliteConnection,
    ) -> Result<bool, Error> {
        use local_settings::dsl as ls;

        let backup_enabled = local_settings::table
            .find(&SINGLETON_ID)
            .select(ls::backup_enabled)
            .first(connection)?;

        Ok(backup_enabled)
    }

    pub fn set_backup_enabled(
        connection: &mut SqliteConnection,
        backup_enabled: bool,
    ) -> Result<(), Error> {
        use local_settings::dsl as ls;

        diesel::update(local_settings::table.find(&SINGLETON_ID))
            .set(ls::backup_enabled.eq(backup_enabled))
            .execute(connection)?;

        Ok(())
    }

    pub fn fetch_kdf_nonce(
        connection: &mut SqliteConnection,
    ) -> Result<Option<KdfNonce>, Error> {
        use local_settings::dsl as ls;

        let nonce_bytes: Option<Vec<u8>> = local_settings::table
            .find(&SINGLETON_ID)
            .select(ls::backup_kdf_nonce)
            .first(connection)?;

        match nonce_bytes {
            Some(nonce) => {
                let nonce: KdfNonce = nonce.try_into()?;
                Ok(Some(nonce))
            }
            None => Ok(None),
        }
    }

    pub fn set_backup_kdf_nonce(
        connection: &mut SqliteConnection,
        kdf_nonce: Option<&KdfNonce>,
    ) -> Result<(), Error> {
        use local_settings::dsl as ls;

        diesel::update(local_settings::table.find(&SINGLETON_ID))
            .set(ls::backup_kdf_nonce.eq(kdf_nonce.map(|n| n.as_ref())))
            .execute(connection)?;

        Ok(())
    }

    pub fn update_backup_timestamp(
        connection: &mut SqliteConnection,
    ) -> Result<(), Error> {
        use local_settings::dsl as ls;

        diesel::update(local_settings::table.find(&SINGLETON_ID))
            .set(ls::backup_completed_at.eq(rfc3339_timestamp()))
            .execute(connection)?;

        Ok(())
    }

    pub fn update_backup_password_timestamp(
        connection: &mut SqliteConnection,
    ) -> Result<(), Error> {
        use local_settings::dsl as ls;

        diesel::update(local_settings::table.find(&SINGLETON_ID))
            .set(ls::backup_password_updated_at.eq(rfc3339_timestamp()))
            .execute(connection)?;

        Ok(())
    }
}
