use std::error::Error;

use uuid::Uuid;
use chrono::{Utc, DateTime};
use bytes::Bytes;
use sha2::{Sha256, Digest};
use log::warn;
use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;

use crate::coingecko::CoinDominanceResponse;
use crate::db::models::{ProvenanceId, RequestMetadata, ResponseMetadata};

pub async fn insert_provenance(
    timestamp: DateTime<Utc>,
    agent_name: &str,
    buffer: &Bytes,
    mime: Option<String>,
    request_meta: &Option<serde_json::Value>,
    response_meta: &Option<serde_json::Value>,
    pool: &PgPool,
) -> Result<ProvenanceId, Box<dyn Error>> {

    let mut tx = pool.begin().await?;

    let mut hasher = Sha256::new();
    hasher.update(buffer.as_ref());
    let hash = hasher.finalize();

    let storage = sqlx::query!(r#"
        with new_obj as (
            insert into object_storage (sha256, data, mime)
            values ($1, $2, $3)
            on conflict (sha256) do update
                set mime = $3
            returning id
        )
        select id from new_obj
        union
        select id from object_storage where sha256 = $1
        "#,
        hash.as_slice(),
        buffer.as_ref(),
        mime)
        .fetch_one(&mut tx)
        .await?;

    let object_id_opt: Option<i64> = storage.id;
    let object_id = object_id_opt.ok_or(sqlx::Error::RowNotFound)?;
    let uuid = Uuid::new_v4();

    sqlx::query!(r#"
        insert into provenance (
            uuid,
            object_id,
            agent,
            timestamp_utc,
            request_metadata,
            response_metadata
        )
        values ($1, $2, $3, $4, $5, $6)
    "#,
        uuid,
        storage.id,
        agent_name,
        timestamp.naive_utc(),
        *request_meta,
        *response_meta)
        .execute(&mut tx)
        .await?;

    tx.commit().await?;
    Ok(ProvenanceId { uuid, object_id })
}

pub async fn insert_snapshot<'a>(
    timestamp: DateTime<Utc>,
    agent_name: &str,
    buffer: &Bytes,
    mime: Option<String>,
    request_meta: &RequestMetadata,
    response_meta: &ResponseMetadata,
    json: Result<CoinDominanceResponse, impl Error + 'static>,
    pool: &PgPool) -> Result<ProvenanceId, Box<dyn Error>> {

    // Log the HTTP response before we try to resolve the result so that we
    // can track any errors even if we're not getting JSON back.
    //
    let pid = insert_data_origin_from_http(
        timestamp,
        agent_name,
        buffer,
        mime,
        request_meta,
        response_meta,
        pool,
    ).await?;

    let json = json.map_err(Box::new)?;

    let mut tx = pool.begin().await?;

    for coin in json.data.iter() {
        sqlx::query!(r#"
        insert into coin_dominance (
            provenance_uuid,
            object_id,
            agent,
            timestamp_utc,
            coin_id,
            coin_name,
            market_cap_usd,
            market_dominance_percentage
        )
        values ($1, $2, $3, $4, $5, $6, $7, $8)
        on conflict do nothing
        "#,
        pid.uuid,
        pid.object_id,
        agent_name,
        json.timestamp.naive_utc(),
        coin.id,
        coin.name,
        coin.market_cap_usd,
        coin.dominance_percentage)
            .execute(&mut tx)
            .await?;
    }

    tx.commit().await?;
    Ok(pid)
}

pub async fn insert_data_origin_from_http(
    timestamp: DateTime<Utc>,
    agent_name: &str,
    buffer: &Bytes,
    mime: Option<String>,
    request_meta: &RequestMetadata,
    response_meta: &ResponseMetadata,
    pool: &PgPool
) -> Result<ProvenanceId, Box<dyn Error>> {

    fn convert<T: Serialize>(value: T) -> Option<Value> {
        let json = serde_json::to_value(value);
        if let Err(ref err) = json {
            warn!("Failed to serialize snapshot metadata: {}", err);
        }
        json.ok()
    }

    let request_meta = convert(request_meta);
    let response_meta = convert(response_meta);

    let pid = insert_provenance(
        timestamp,
        agent_name,
        buffer,
        mime,
        &request_meta,
        &response_meta,
        pool
    ).await?;

    Ok(pid)
}
