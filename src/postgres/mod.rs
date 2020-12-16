use std::collections::BTreeMap;
use std::fmt::{self, Debug, Formatter};
use std::sync::Arc;

use async_stream::try_stream;

use futures_lite::stream::StreamExt;

use sqlx::{
    pool::PoolConnection,
    postgres::{PgPool, Postgres},
    Done, Row,
};

use super::db_utils::{
    decode_tags, encode_store_key, encode_tag_filter, expiry_timestamp, extend_query,
    random_profile_name, replace_arg_placeholders, DbSession, DbSessionActive, DbSessionRef,
    ExtDatabase, QueryParams, QueryPrepare, PAGE_SIZE,
};
use super::error::Result;
use super::future::{unblock, unblock_scoped, BoxFuture};
use super::keys::{store::StoreKey, wrap::WrapKeyMethod, EntryEncryptor, KeyCache, PassKey};
use super::store::{Backend, QueryBackend, Scan};
use super::types::{EncEntryTag, Entry, EntryKind, EntryOperation, EntryTag, ProfileId, TagFilter};

const COUNT_QUERY: &'static str = "SELECT COUNT(*) FROM items i
    WHERE profile_id = $1 AND kind = $2 AND category = $3
    AND (expiry IS NULL OR expiry > CURRENT_TIMESTAMP)";
const DELETE_QUERY: &'static str = "DELETE FROM items
    WHERE profile_id = $1 AND kind = $2 AND category = $3 AND name = $4";
const FETCH_QUERY: &'static str = "SELECT id, value,
    (SELECT ARRAY_TO_STRING(ARRAY_AGG(it.plaintext || ':'
        || ENCODE(it.name, 'hex') || ':' || ENCODE(it.value, 'hex')), ',')
        FROM items_tags it WHERE it.item_id = i.id) tags
    FROM items i
    WHERE profile_id = $1 AND kind = $2 AND category = $3 AND name = $4
    AND (expiry IS NULL OR expiry > CURRENT_TIMESTAMP)";
const FETCH_QUERY_UPDATE: &'static str = "SELECT id, value,
    (SELECT ARRAY_TO_STRING(ARRAY_AGG(it.plaintext || ':'
        || ENCODE(it.name, 'hex') || ':' || ENCODE(it.value, 'hex')), ',')
        FROM items_tags it WHERE it.item_id = i.id) tags
    FROM items i
    WHERE profile_id = $1 AND kind = $2 AND category = $3 AND name = $4
    AND (expiry IS NULL OR expiry > CURRENT_TIMESTAMP) FOR UPDATE";
const INSERT_QUERY: &'static str =
    "INSERT INTO items (profile_id, kind, category, name, value, expiry)
    VALUES ($1, $2, $3, $4, $5, $6)
    ON CONFLICT DO NOTHING RETURNING id";
const SCAN_QUERY: &'static str = "SELECT id, name, value,
    (SELECT ARRAY_TO_STRING(ARRAY_AGG(it.plaintext || ':'
        || ENCODE(it.name, 'hex') || ':' || ENCODE(it.value, 'hex')), ',')
        FROM items_tags it WHERE it.item_id = i.id) tags
    FROM items i WHERE profile_id = $1 AND kind = $2 AND category = $3
    AND (expiry IS NULL OR expiry > CURRENT_TIMESTAMP)";
const DELETE_ALL_QUERY: &'static str = "DELETE FROM items i
    WHERE i.profile_id = $1 AND i.kind = $2 AND i.category = $3";
const TAG_INSERT_QUERY: &'static str = "INSERT INTO items_tags
    (item_id, name, value, plaintext) VALUES ($1, $2, $3, $4)";

mod provision;
pub use provision::PostgresStoreOptions;

#[cfg(feature = "pg_test")]
pub mod test_db;

pub struct PostgresStore {
    conn_pool: PgPool,
    default_profile: String,
    key_cache: Arc<KeyCache>,
    host: String,
    name: String,
}

impl PostgresStore {
    pub(crate) fn new(
        conn_pool: PgPool,
        default_profile: String,
        key_cache: KeyCache,
        host: String,
        name: String,
    ) -> Self {
        Self {
            conn_pool,
            default_profile,
            key_cache: Arc::new(key_cache),
            host,
            name,
        }
    }
}

impl Backend for PostgresStore {
    type Session = DbSession<Postgres>;

    fn create_profile(&self, name: Option<String>) -> BoxFuture<Result<String>> {
        let name = name.unwrap_or_else(random_profile_name);
        Box::pin(async move {
            let key = StoreKey::new()?;
            let enc_key = key.to_string()?;
            let mut conn = self.conn_pool.acquire().await?;
            if let Some(pid) = sqlx::query_scalar(
                "INSERT INTO profiles (name, store_key) VALUES ($1, $2) 
                ON CONFLICT DO NOTHING RETURNING id",
            )
            .bind(&name)
            .bind(enc_key.as_bytes())
            .fetch_optional(&mut conn)
            .await?
            {
                self.key_cache
                    .add_profile(name.clone(), pid, Arc::new(key))
                    .await;
                Ok(name)
            } else {
                Err(err_msg!(Duplicate, "Duplicate profile name"))
            }
        })
    }

    fn get_profile_name(&self) -> &str {
        self.default_profile.as_str()
    }

    fn remove_profile(&self, name: String) -> BoxFuture<Result<bool>> {
        Box::pin(async move {
            let mut conn = self.conn_pool.acquire().await?;
            Ok(sqlx::query("DELETE FROM profiles WHERE name=$1")
                .bind(&name)
                .execute(&mut conn)
                .await?
                .rows_affected()
                != 0)
        })
    }

    fn rekey_backend(
        &mut self,
        method: WrapKeyMethod,
        pass_key: PassKey<'_>,
    ) -> BoxFuture<Result<()>> {
        let pass_key = pass_key.into_owned();
        Box::pin(async move {
            let (wrap_key, wrap_key_ref) = unblock(move || method.resolve(pass_key)).await?;
            let mut txn = self.conn_pool.begin().await?;
            let mut rows = sqlx::query("SELECT id, store_key FROM profiles").fetch(&mut txn);
            let mut upd_keys = BTreeMap::<ProfileId, Vec<u8>>::new();
            while let Some(row) = rows.next().await {
                let row = row?;
                let pid = row.try_get(0)?;
                let enc_key = row.try_get(1)?;
                let store_key = self.key_cache.load_key(enc_key).await?;
                let upd_key = unblock_scoped(|| encode_store_key(&store_key, &wrap_key)).await?;
                upd_keys.insert(pid, upd_key);
            }
            drop(rows);
            for (pid, key) in upd_keys {
                if sqlx::query("UPDATE profiles SET store_key=$1 WHERE id=$2")
                    .bind(key)
                    .bind(pid)
                    .execute(&mut txn)
                    .await?
                    .rows_affected()
                    != 1
                {
                    return Err(err_msg!(Backend, "Error updating profile store key"));
                }
            }
            if sqlx::query("UPDATE config SET value=$1 WHERE name='wrap_key'")
                .bind(wrap_key_ref.into_uri())
                .execute(&mut txn)
                .await?
                .rows_affected()
                != 1
            {
                return Err(err_msg!(Backend, "Error updating wrap key"));
            }
            txn.commit().await?;
            self.key_cache = Arc::new(KeyCache::new(wrap_key));
            Ok(())
        })
    }

    fn scan(
        &self,
        profile: Option<String>,
        kind: EntryKind,
        category: String,
        tag_filter: Option<TagFilter>,
        offset: Option<i64>,
        limit: Option<i64>,
    ) -> BoxFuture<Result<Scan<'static, Entry>>> {
        Box::pin(async move {
            let session = self.session(profile, false)?;
            perform_scan(
                session.owned_ref(),
                kind,
                category,
                tag_filter,
                offset,
                limit,
                false,
            )
            .await
        })
    }

    fn session(&self, profile: Option<String>, transaction: bool) -> Result<Self::Session> {
        Ok(DbSession::new(
            self.conn_pool.clone(),
            self.key_cache.clone(),
            profile.unwrap_or_else(|| self.default_profile.clone()),
            transaction,
        ))
    }

    fn close(&self) -> BoxFuture<Result<()>> {
        Box::pin(async move {
            self.conn_pool.close().await;
            Ok(())
        })
    }
}

impl Debug for PostgresStore {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostgresStore")
            .field("default_profile", &self.default_profile)
            .field("host", &self.host)
            .field("name", &self.name)
            .finish()
    }
}

impl QueryBackend for DbSession<Postgres> {
    fn count<'q>(
        &'q mut self,
        kind: EntryKind,
        category: &'q str,
        tag_filter: Option<TagFilter>,
    ) -> BoxFuture<'q, Result<i64>> {
        Box::pin(async move {
            let (profile_id, key) = acquire_key(&mut *self).await?;
            let enc_category = unblock_scoped(|| key.encrypt_entry_category(category)).await?;
            let mut params = QueryParams::new();
            params.push(profile_id);
            params.push(kind as i16);
            params.push(enc_category);
            let tag_filter =
                encode_tag_filter::<PostgresStore>(tag_filter, key, params.len()).await?;
            let query =
                extend_query::<PostgresStore>(COUNT_QUERY, &mut params, tag_filter, None, None)?;
            let mut active = acquire_session(&mut *self).await?;
            let count = sqlx::query_scalar_with(query.as_str(), params)
                .fetch_one(active.connection_mut())
                .await?;
            Ok(count)
        })
    }

    fn fetch(
        &mut self,
        kind: EntryKind,
        category: &str,
        name: &str,
        for_update: bool,
    ) -> BoxFuture<Result<Option<Entry>>> {
        let category = category.to_string();
        let name = name.to_string();

        Box::pin(async move {
            let (profile_id, key) = acquire_key(&mut *self).await?;
            let (enc_category, enc_name) = unblock_scoped(|| {
                Result::Ok((
                    key.encrypt_entry_category(&category)?,
                    key.encrypt_entry_name(&name)?,
                ))
            })
            .await?;
            let mut active = acquire_session(&mut *self).await?;
            if let Some(row) = sqlx::query(if for_update && active.is_transaction() {
                FETCH_QUERY_UPDATE
            } else {
                FETCH_QUERY
            })
            .bind(profile_id)
            .bind(kind as i16)
            .bind(&enc_category)
            .bind(&enc_name)
            .fetch_optional(active.connection_mut())
            .await?
            {
                let (value, tags) = unblock_scoped(|| {
                    let value = key.decrypt_entry_value(row.try_get(1)?)?;
                    let tags = if let Some(enc_tags) = row
                        .try_get::<Option<&str>, _>(2)?
                        .map(|t| decode_tags(t.as_bytes()))
                        .transpose()
                        .map_err(|_| err_msg!(Unexpected, "Error decoding entry tags"))?
                    {
                        Some(key.decrypt_entry_tags(&enc_tags)?)
                    } else {
                        Some(vec![])
                    };
                    Result::Ok((value, tags))
                })
                .await?;

                Ok(Some(Entry::new(category, name, value, tags)))
            } else {
                Ok(None)
            }
        })
    }

    fn fetch_all<'q>(
        &'q mut self,
        kind: EntryKind,
        category: &'q str,
        tag_filter: Option<TagFilter>,
        limit: Option<i64>,
        for_update: bool,
    ) -> BoxFuture<'q, Result<Vec<Entry>>> {
        let category = category.to_string();
        Box::pin(async move {
            let for_update = for_update && self.is_transaction();
            let active = self.borrow_mut();
            let mut scan =
                perform_scan(active, kind, category, tag_filter, None, limit, for_update).await?;
            let mut results = vec![];
            loop {
                if let Some(rows) = scan.fetch_next().await? {
                    results.extend(rows);
                } else {
                    break;
                }
            }
            Ok(results)
        })
    }

    fn remove_all<'q>(
        &'q mut self,
        kind: EntryKind,
        category: &'q str,
        tag_filter: Option<TagFilter>,
    ) -> BoxFuture<'q, Result<i64>> {
        Box::pin(async move {
            let (profile_id, key) = acquire_key(&mut *self).await?;
            let enc_category = unblock_scoped(|| key.encrypt_entry_category(&category)).await?;
            let mut params = QueryParams::new();
            params.push(profile_id);
            params.push(kind as i16);
            params.push(enc_category);
            let tag_filter =
                encode_tag_filter::<PostgresStore>(tag_filter, key, params.len()).await?;
            let query = extend_query::<PostgresStore>(
                DELETE_ALL_QUERY,
                &mut params,
                tag_filter,
                None,
                None,
            )?;

            let mut active = acquire_session(&mut *self).await?;
            let removed = sqlx::query_with(query.as_str(), params)
                .execute(active.connection_mut())
                .await?
                .rows_affected();
            Ok(removed as i64)
        })
    }

    fn update<'q>(
        &'q mut self,
        kind: EntryKind,
        operation: EntryOperation,
        category: &'q str,
        name: &'q str,
        value: Option<&'q [u8]>,
        tags: Option<&'q [EntryTag]>,
        expiry_ms: Option<i64>,
    ) -> BoxFuture<'q, Result<()>> {
        Box::pin(async move {
            let (_, key) = acquire_key(&mut *self).await?;

            match operation {
                EntryOperation::Insert => {
                    let (enc_category, enc_name, enc_value, enc_tags) = unblock_scoped(|| {
                        Result::Ok((
                            key.encrypt_entry_category(&category)?,
                            key.encrypt_entry_name(&name)?,
                            key.encrypt_entry_value(value.unwrap())?,
                            tags.map(|t| key.encrypt_entry_tags(t)).transpose()?,
                        ))
                    })
                    .await?;
                    let mut active = acquire_session(&mut *self).await?;
                    let mut txn = active.as_transaction().await?;
                    perform_insert(
                        &mut txn,
                        kind,
                        &enc_category,
                        &enc_name,
                        &enc_value,
                        enc_tags,
                        expiry_ms,
                    )
                    .await?;
                    txn.commit().await?;
                    Ok(())
                }

                EntryOperation::Replace => {
                    let (enc_category, enc_name, enc_value, enc_tags) = unblock_scoped(|| {
                        Result::Ok((
                            key.encrypt_entry_category(&category)?,
                            key.encrypt_entry_name(&name)?,
                            key.encrypt_entry_value(value.unwrap())?,
                            tags.map(|t| key.encrypt_entry_tags(t)).transpose()?,
                        ))
                    })
                    .await?;
                    let mut active = acquire_session(&mut *self).await?;
                    let mut txn = active.as_transaction().await?;
                    perform_remove(&mut txn, kind, &enc_category, &enc_name, false).await?;
                    perform_insert(
                        &mut txn,
                        kind,
                        &enc_category,
                        &enc_name,
                        &enc_value,
                        enc_tags,
                        expiry_ms,
                    )
                    .await?;
                    txn.commit().await?;
                    Ok(())
                }

                EntryOperation::Remove => {
                    let (enc_category, enc_name) = unblock_scoped(|| {
                        Result::Ok((
                            key.encrypt_entry_category(&category)?,
                            key.encrypt_entry_name(&name)?,
                        ))
                    })
                    .await?;
                    let mut active = acquire_session(&mut *self).await?;
                    Ok(perform_remove(&mut active, kind, &enc_category, &enc_name, false).await?)
                }
            }
        })
    }

    fn close(self, commit: bool) -> BoxFuture<'static, Result<()>> {
        Box::pin(DbSession::close(self, commit))
    }
}

impl ExtDatabase for Postgres {}

impl QueryPrepare for PostgresStore {
    type DB = Postgres;

    fn placeholder(index: i64) -> String {
        format!("${}", index)
    }

    fn limit_query<'q>(
        mut query: String,
        args: &mut QueryParams<'q, Self::DB>,
        offset: Option<i64>,
        limit: Option<i64>,
    ) -> String
    where
        i64: for<'e> sqlx::Encode<'e, Self::DB> + sqlx::Type<Self::DB>,
    {
        if offset.is_some() || limit.is_some() {
            let last_idx = (args.len() + 1) as i64;
            args.push(limit);
            args.push(offset.unwrap_or(0));
            let limit = replace_arg_placeholders::<Self>(" LIMIT $$ OFFSET $$", last_idx);
            query.push_str(&limit);
        }
        query
    }
}

async fn acquire_key(session: &mut DbSession<Postgres>) -> Result<(ProfileId, Arc<StoreKey>)> {
    if let Some(ret) = session.profile_and_key() {
        Ok(ret)
    } else {
        session.make_active(&resolve_profile_key).await?;
        Ok(session.profile_and_key().unwrap())
    }
}

async fn acquire_session<'q>(
    session: &'q mut DbSession<Postgres>,
) -> Result<DbSessionActive<'q, Postgres>> {
    session.make_active(&resolve_profile_key).await
}

async fn resolve_profile_key(
    conn: &mut PoolConnection<Postgres>,
    cache: Arc<KeyCache>,
    profile: String,
) -> Result<(ProfileId, Arc<StoreKey>)> {
    if let Some((pid, key)) = cache.get_profile(profile.as_str()).await {
        Ok((pid, key))
    } else {
        if let Some(row) = sqlx::query("SELECT id, store_key FROM profiles WHERE name=?1")
            .bind(profile.as_str())
            .fetch_optional(conn)
            .await?
        {
            let pid = row.try_get(0)?;
            let key = Arc::new(cache.load_key(row.try_get(1)?).await?);
            cache.add_profile(profile, pid, key.clone()).await;
            Ok((pid, key))
        } else {
            Err(err_msg!(NotFound, "Profile not found"))
        }
    }
}

async fn perform_insert<'q>(
    active: &mut DbSessionActive<'q, Postgres>,
    kind: EntryKind,
    enc_category: &[u8],
    enc_name: &[u8],
    enc_value: &[u8],
    enc_tags: Option<Vec<EncEntryTag>>,
    expiry_ms: Option<i64>,
) -> Result<()> {
    trace!("Insert entry");
    let row_id: i64 = sqlx::query_scalar(INSERT_QUERY)
        .bind(active.profile_id)
        .bind(kind as i16)
        .bind(enc_category)
        .bind(enc_name)
        .bind(enc_value)
        .bind(expiry_ms.map(expiry_timestamp).transpose()?)
        .fetch_optional(active.connection_mut())
        .await?
        .ok_or_else(|| err_msg!(Duplicate, "Duplicate row"))?;
    if let Some(tags) = enc_tags {
        for tag in tags {
            sqlx::query(TAG_INSERT_QUERY)
                .bind(row_id)
                .bind(&tag.name)
                .bind(&tag.value)
                .bind(tag.plaintext as i16)
                .execute(active.connection_mut())
                .await?;
        }
    }
    Ok(())
}

async fn perform_remove<'q>(
    active: &mut DbSessionActive<'q, Postgres>,
    kind: EntryKind,
    enc_category: &[u8],
    enc_name: &[u8],
    ignore_error: bool,
) -> Result<()> {
    trace!("Remove entry");
    let done = sqlx::query(DELETE_QUERY)
        .bind(active.profile_id)
        .bind(kind as i16)
        .bind(enc_category)
        .bind(enc_name)
        .execute(active.connection_mut())
        .await?;
    if done.rows_affected() == 0 && !ignore_error {
        Err(err_msg!(NotFound, "Entry not found"))
    } else {
        Ok(())
    }
}

async fn perform_scan<'q>(
    mut active: DbSessionRef<'q, Postgres>,
    kind: EntryKind,
    category: String,
    tag_filter: Option<TagFilter>,
    offset: Option<i64>,
    limit: Option<i64>,
    for_update: bool,
) -> Result<Scan<'q, Entry>> {
    let (profile_id, key) = acquire_key(&mut *active).await?;
    let enc_category = unblock_scoped(|| key.encrypt_entry_category(&category)).await?;

    let scan = try_stream! {
        let mut params = QueryParams::new();
        params.push(profile_id);
        params.push(kind as i16);
        params.push(enc_category);
        let tag_filter = encode_tag_filter::<PostgresStore>(tag_filter, key.clone(), params.len()).await?;
        let mut query = extend_query::<PostgresStore>(SCAN_QUERY, &mut params, tag_filter, offset, limit)?;
        if for_update {
            query.push_str(" FOR UPDATE");
        }
        let mut batch = Vec::with_capacity(PAGE_SIZE);

        let mut acquired = acquire_session(&mut *active).await?;
        let mut rows = sqlx::query_with(query.as_str(), params).fetch(acquired.connection_mut());
        while let Some(row) = rows.next().await {
            let row = row?;
            let (name, value, tags) = unblock_scoped(|| {
                let name = key.decrypt_entry_name(row.try_get(1)?)?;
                let value = key.decrypt_entry_value(row.try_get(2)?)?;
                let tags = if let Some(enc_tags) = row
                .try_get::<Option<&str>, _>(3)?
                .map(|t| decode_tags(t.as_bytes()))
                .transpose()
                .map_err(|_| err_msg!(Unexpected, "Error decoding entry tags"))?
            {
                Some(key.decrypt_entry_tags(&enc_tags)?)
            } else {
                Some(vec![])
            };
                Result::Ok((name, value, tags))
            })
            .await?;

            batch.push(Entry::new(
                category.clone(),
                name,
                value,
                tags,
            ));
            if batch.len() == PAGE_SIZE {
                yield batch.split_off(0);
            }
        }
        drop(rows);
        drop(acquired);
        drop(active);

        if batch.len() > 0 {
            yield batch;
        }
    };
    Ok(Scan::new(scan, PAGE_SIZE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db_utils::replace_arg_placeholders;

    #[test]
    fn postgres_simple_and_convert_args_works() {
        assert_eq!(
            &replace_arg_placeholders::<PostgresStore>("This $$ is $10 a $$ string!", 3),
            "This $3 is $12 a $5 string!",
        );
    }
}
