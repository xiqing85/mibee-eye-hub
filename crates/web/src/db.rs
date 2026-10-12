use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{SqlitePool, sqlite::SqliteConnectOptions};
use std::path::Path;

/// A camera row as stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraRow {
    pub id: String,
    pub name: String,
    pub camera_type: String,
    pub config: serde_json::Value,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    /// When the physical device went offline (NULL when online).
    /// Set by the hot-plug monitor when udev detects device removal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offline_since: Option<String>,
}

/// One persisted hearing record (SPEC appendix A #24): what an audio engine
/// recognized, as text. `kind` is `"sound"` (YAMNet class in `text`, voted
/// score in `score`) or `"voice"` (transcript in `text`, wake word in
/// `keyword`).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct HearingRecord {
    pub id: i64,
    pub kind: String,
    pub text: String,
    pub score: Option<f64>,
    pub keyword: String,
    pub timestamp_ms: i64,
    /// Best-matching enrolled speaker for voice records ("" = unknown;
    /// sound records always "").
    pub speaker: String,
    /// 【画面】 summary captured when the event fired (#30-C; "" when
    /// nothing was known / pre-migration rows).
    pub scene: String,
    /// MP4 segment covering the event timestamp when the camera was
    /// recording (#30-C; "" when recording off / rotation race).
    pub media_ref: String,
}

/// FIFO cap applied on every insert so a chatty microphone can never grow
/// the table without bound.
pub const HEARING_RECORDS_CAP: i64 = 1000;

/// Persist one hearing record. Fail-open at the call site: a full or busy
/// database must never take the audio pipeline down, so callers log the
/// error and move on.
#[allow(clippy::too_many_arguments)]
pub async fn insert_hearing_record(
    pool: &SqlitePool,
    kind: &str,
    text: &str,
    score: Option<f64>,
    keyword: &str,
    speaker: &str,
    scene: &str,
    media_ref: &str,
    timestamp_ms: i64,
) -> Result<()> {
    let mut tx = pool.begin().await.context("hearing record: begin")?;
    sqlx::query(
        "INSERT INTO hearing_records (kind, text, score, keyword, speaker, scene, media_ref, timestamp_ms)          VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
        .bind(kind)
        .bind(text)
        .bind(score)
        .bind(keyword)
        .bind(speaker)
        .bind(scene)
        .bind(media_ref)
        .bind(timestamp_ms)
        .execute(&mut *tx)
        .await
        .context("hearing record: insert")?;
    sqlx::query(&format!(
        "DELETE FROM hearing_records WHERE id NOT IN \
         (SELECT id FROM hearing_records ORDER BY id DESC LIMIT {HEARING_RECORDS_CAP})"
    ))
    .execute(&mut *tx)
    .await
    .context("hearing record: prune")?;
    tx.commit().await.context("hearing record: commit")
}

/// Row shape fetched for [`HearingRecord`] (sqlx runtime queries decode
/// into tuples before mapping).
type HearingRow = (
    i64,
    String,
    String,
    Option<f64>,
    String,
    String,
    String,
    String,
    i64,
);

/// List hearing records, newest first. `kind` filters when given (must be
/// `"sound"` or `"voice"`; anything else is treated as no filter).
pub async fn list_hearing_records(
    pool: &SqlitePool,
    limit: i64,
    kind: Option<&str>,
) -> Result<Vec<HearingRecord>> {
    let limit = limit.clamp(1, 500);
    let rows: Vec<HearingRow> = if matches!(kind, Some("sound") | Some("voice")) {
        sqlx::query_as(
            "SELECT id, kind, text, score, keyword, speaker, scene, media_ref, timestamp_ms \
             FROM hearing_records WHERE kind = ?1 ORDER BY timestamp_ms DESC, id DESC LIMIT ?2",
        )
        .bind(kind)
        .bind(limit)
        .fetch_all(pool)
        .await
        .context("hearing record: list")?
    } else {
        sqlx::query_as(
            "SELECT id, kind, text, score, keyword, speaker, scene, media_ref, timestamp_ms \
             FROM hearing_records ORDER BY timestamp_ms DESC, id DESC LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(pool)
        .await
        .context("hearing record: list")?
    };
    Ok(rows
        .into_iter()
        .map(
            |(id, kind, text, score, keyword, speaker, scene, media_ref, timestamp_ms)| {
                HearingRecord {
                    id,
                    kind,
                    text,
                    score,
                    keyword,
                    speaker,
                    scene,
                    media_ref,
                    timestamp_ms,
                }
            },
        )
        .collect())
}

/// Delete every hearing record; returns the number of rows removed.
pub async fn clear_hearing_records(pool: &SqlitePool) -> Result<usize> {
    let result = sqlx::query("DELETE FROM hearing_records")
        .execute(pool)
        .await
        .context("hearing record: clear")?;
    Ok(result.rows_affected() as usize)
}

// ---------------------------------------------------------------------------
// Conversation records (SPEC v1 §3.4)
// ---------------------------------------------------------------------------

use crate::conversations::ConversationTurn;
use crate::conversations::TurnDraft;

/// FIFO cap applied on every insert (SPEC §3.4 storage semantics).
pub const CONVERSATION_TURNS_CAP: i64 = 1000;

/// Persist one finished dialogue turn, FIFO-pruning to `cap` rows, and
/// return the row as stored (with its id). Fail-open at the call site:
/// callers log the error and move on — the conversation pipeline is
/// never touched by recording failures.
pub async fn insert_conversation_turn_with_cap(
    pool: &SqlitePool,
    draft: TurnDraft,
    cap: i64,
) -> Result<ConversationTurn> {
    let turn = draft.into_turn(0);
    let thinking_json = serde_json::to_string(&turn.thinking).unwrap_or_else(|_| "[]".to_string());
    let mut tx = pool.begin().await.context("conversation turn: begin")?;
    sqlx::query(
        "INSERT INTO conversation_turns (conversation_id, origin, started_ms, user_text, thinking_json, reply_text, engine, interrupted) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(&turn.conversation_id)
    .bind(&turn.origin)
    .bind(turn.started_ms)
    .bind(&turn.user_text)
    .bind(&thinking_json)
    .bind(&turn.reply_text)
    .bind(&turn.engine)
    .bind(turn.interrupted)
    .execute(&mut *tx)
    .await
    .context("conversation turn: insert")?;
    let (id,): (i64,) = sqlx::query_as("SELECT last_insert_rowid()")
        .fetch_one(&mut *tx)
        .await
        .context("conversation turn: rowid")?;
    sqlx::query(&format!(
        "DELETE FROM conversation_turns WHERE id NOT IN \
         (SELECT id FROM conversation_turns ORDER BY id DESC LIMIT {cap})"
    ))
    .execute(&mut *tx)
    .await
    .context("conversation turn: prune")?;
    tx.commit().await.context("conversation turn: commit")?;
    Ok(ConversationTurn { id, ..turn })
}

/// [`insert_conversation_turn_with_cap`] at the production cap.
pub async fn insert_conversation_turn(
    pool: &SqlitePool,
    draft: TurnDraft,
) -> Result<ConversationTurn> {
    insert_conversation_turn_with_cap(pool, draft, CONVERSATION_TURNS_CAP).await
}

/// Row shape for [`ConversationTurn`] (thinking is stored as JSON).
type ConversationTurnRow = (
    i64,
    String,
    String,
    i64,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    bool,
);

/// List conversation turns, newest first (SPEC §3.4: `limit` default 50,
/// cap 200). Thinking entries decode from their JSON column.
pub async fn list_conversation_turns(
    pool: &SqlitePool,
    limit: i64,
) -> Result<Vec<ConversationTurn>> {
    let limit = limit.clamp(1, 200);
    let rows: Vec<ConversationTurnRow> = sqlx::query_as(
        "SELECT id, conversation_id, origin, started_ms, user_text, thinking_json, reply_text, engine, interrupted \
         FROM conversation_turns ORDER BY id DESC LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("conversation turn: list")?;
    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                conversation_id,
                origin,
                started_ms,
                user_text,
                thinking_json,
                reply_text,
                engine,
                interrupted,
            )| {
                ConversationTurn {
                    id,
                    conversation_id,
                    origin,
                    started_ms,
                    user_text,
                    thinking: serde_json::from_str(&thinking_json).unwrap_or_default(),
                    reply_text,
                    engine,
                    interrupted,
                }
            },
        )
        .collect())
}

/// Delete every conversation turn; returns the number of rows removed
/// (SPEC §3.4 clear-all, `hearing_records` precedent).
pub async fn clear_conversation_turns(pool: &SqlitePool) -> Result<usize> {
    let result = sqlx::query("DELETE FROM conversation_turns")
        .execute(pool)
        .await
        .context("conversation turn: clear")?;
    Ok(result.rows_affected() as usize)
}

// ---------------------------------------------------------------------------
// Away mode event records (SPEC v1 §3.6, appendix A #44)
// ---------------------------------------------------------------------------

use crate::away::AwayEventRecord;

/// FIFO cap applied on every away-event insert (SPEC §3.6 storage).
pub const AWAY_EVENTS_CAP: i64 = 1000;

/// Insert one away event, FIFO-prune to `cap` rows, and return the row
/// as stored plus the snapshot file names the prune orphaned (the
/// caller unlinks them — files are not the database's business).
pub async fn insert_away_event_with_cap(
    pool: &SqlitePool,
    record: &AwayEventRecord,
    cap: i64,
) -> Result<(AwayEventRecord, Vec<String>)> {
    let mut tx = pool.begin().await.context("away event: begin")?;
    sqlx::query(
        "INSERT INTO away_events (camera_id, kind, started_ms, labels, face_name, description, visitor_reply, snapshot, state) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(&record.camera_id)
    .bind(&record.kind)
    .bind(record.started_ms)
    .bind(&record.labels)
    .bind(&record.face_name)
    .bind(&record.description)
    .bind(&record.visitor_reply)
    .bind(&record.snapshot)
    .bind(&record.state)
    .execute(&mut *tx)
    .await
    .context("away event: insert")?;
    let (id,): (i64,) = sqlx::query_as("SELECT last_insert_rowid()")
        .fetch_one(&mut *tx)
        .await
        .context("away event: rowid")?;
    // Names of the rows the prune is about to drop, so the caller can
    // unlink their snapshot files.
    let pruned: Vec<(String,)> = sqlx::query_as(
        "SELECT snapshot FROM away_events WHERE id NOT IN \
         (SELECT id FROM away_events ORDER BY id DESC LIMIT ?1) AND snapshot IS NOT NULL",
    )
    .bind(cap)
    .fetch_all(&mut *tx)
    .await
    .context("away event: pruned names")?;
    sqlx::query(&format!(
        "DELETE FROM away_events WHERE id NOT IN \
         (SELECT id FROM away_events ORDER BY id DESC LIMIT {cap})"
    ))
    .execute(&mut *tx)
    .await
    .context("away event: prune")?;
    tx.commit().await.context("away event: commit")?;
    Ok((
        AwayEventRecord {
            id,
            camera_id: record.camera_id.clone(),
            kind: record.kind.clone(),
            started_ms: record.started_ms,
            labels: record.labels.clone(),
            face_name: record.face_name.clone(),
            description: record.description.clone(),
            visitor_reply: record.visitor_reply.clone(),
            snapshot: record.snapshot.clone(),
            state: record.state.clone(),
        },
        pruned.into_iter().map(|(s,)| s).collect(),
    ))
}

/// [`insert_away_event_with_cap`] at the production cap.
pub async fn insert_away_event(
    pool: &SqlitePool,
    record: &AwayEventRecord,
) -> Result<(AwayEventRecord, Vec<String>)> {
    insert_away_event_with_cap(pool, record, AWAY_EVENTS_CAP).await
}

type AwayEventRow = (
    i64,
    String,
    String,
    i64,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
);

fn away_row_to_record(r: AwayEventRow) -> AwayEventRecord {
    AwayEventRecord {
        id: r.0,
        camera_id: r.1,
        kind: r.2,
        started_ms: r.3,
        labels: r.4,
        face_name: r.5,
        description: r.6,
        visitor_reply: r.7,
        snapshot: r.8,
        state: r.9,
    }
}

const AWAY_EVENT_COLUMNS: &str = "id, camera_id, kind, started_ms, labels, face_name, description, visitor_reply, snapshot, state";

/// List away events, newest first (SPEC §3.6: `limit` default 50,
/// cap 200).
pub async fn list_away_events(pool: &SqlitePool, limit: i64) -> Result<Vec<AwayEventRecord>> {
    let limit = limit.clamp(1, 200);
    let rows: Vec<AwayEventRow> = sqlx::query_as(&format!(
        "SELECT {AWAY_EVENT_COLUMNS} FROM away_events ORDER BY id DESC LIMIT ?1"
    ))
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("away event: list")?;
    Ok(rows.into_iter().map(away_row_to_record).collect())
}

/// Fetch one away event (snapshot endpoint join key).
pub async fn get_away_event(pool: &SqlitePool, id: i64) -> Result<Option<AwayEventRecord>> {
    let row: Option<AwayEventRow> = sqlx::query_as(&format!(
        "SELECT {AWAY_EVENT_COLUMNS} FROM away_events WHERE id = ?1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("away event: get")?;
    Ok(row.map(away_row_to_record))
}

/// Patch one column and return the row as stored (the SSE update
/// carries the full record); `None` when the row vanished. `column` is
/// a call-site constant, never client input.
async fn patch_away_field(
    pool: &SqlitePool,
    id: i64,
    column: &str,
    value: &str,
) -> Result<Option<AwayEventRecord>> {
    let rows = sqlx::query_as::<_, AwayEventRow>(&format!(
        "UPDATE away_events SET {column} = ?1 WHERE id = ?2 RETURNING {AWAY_EVENT_COLUMNS}"
    ))
    .bind(value)
    .bind(id)
    .fetch_all(pool)
    .await
    .context("away event: patch")?;
    Ok(rows.into_iter().next().map(away_row_to_record))
}

/// VLM description arrived asynchronously (SPEC §3.6) — patch + return.
pub async fn update_away_description(
    pool: &SqlitePool,
    id: i64,
    description: &str,
) -> Result<Option<AwayEventRecord>> {
    patch_away_field(pool, id, "description", description).await
}

/// Visitor answered inside the listen window — record the reply and
/// flip the state to `answered` (SPEC §3.6 state machine).
pub async fn update_away_reply(
    pool: &SqlitePool,
    id: i64,
    reply: &str,
) -> Result<Option<AwayEventRecord>> {
    let rows = sqlx::query_as::<_, AwayEventRow>(&format!(
        "UPDATE away_events SET visitor_reply = ?1, state = 'answered' WHERE id = ?2 \
         RETURNING {AWAY_EVENT_COLUMNS}"
    ))
    .bind(reply)
    .bind(id)
    .fetch_all(pool)
    .await
    .context("away event: reply")?;
    Ok(rows.into_iter().next().map(away_row_to_record))
}

/// Terminal state transition (greeting → listening, listening → silent…).
pub async fn update_away_state(
    pool: &SqlitePool,
    id: i64,
    state: &str,
) -> Result<Option<AwayEventRecord>> {
    patch_away_field(pool, id, "state", state).await
}

/// Clear every away event; returns the removed count and the snapshot
/// names to unlink (SPEC §3.6 DELETE).
pub async fn clear_away_events(pool: &SqlitePool) -> Result<(usize, Vec<String>)> {
    let mut tx = pool.begin().await.context("away event: clear begin")?;
    let names: Vec<(String,)> =
        sqlx::query_as("SELECT snapshot FROM away_events WHERE snapshot IS NOT NULL")
            .fetch_all(&mut *tx)
            .await
            .context("away event: clear names")?;
    let result = sqlx::query("DELETE FROM away_events")
        .execute(&mut *tx)
        .await
        .context("away event: clear")?;
    tx.commit().await.context("away event: clear commit")?;
    Ok((
        result.rows_affected() as usize,
        names.into_iter().map(|(s,)| s).collect(),
    ))
}

/// `(total, person)` events inside one armed window (`started_ms >=
/// since`) — the disarm summary's counts (SPEC §3.6 `summary`).
pub async fn away_stats_since(pool: &SqlitePool, since_ms: i64) -> Result<(i64, i64)> {
    let (total,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM away_events WHERE started_ms >= ?1")
            .bind(since_ms)
            .fetch_one(pool)
            .await
            .context("away stats since: total")?;
    let (visitors,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM away_events WHERE started_ms >= ?1 AND kind = 'person'",
    )
    .bind(since_ms)
    .fetch_one(pool)
    .await
    .context("away stats since: visitors")?;
    Ok((total, visitors))
}

/// Events inside one armed window, newest first (summary prompt input).
pub async fn list_away_events_since(
    pool: &SqlitePool,
    since_ms: i64,
    limit: i64,
) -> Result<Vec<AwayEventRecord>> {
    let limit = limit.clamp(1, 50);
    let rows: Vec<AwayEventRow> = sqlx::query_as(&format!(
        "SELECT {AWAY_EVENT_COLUMNS} FROM away_events WHERE started_ms >= ?1          ORDER BY id DESC LIMIT ?2"
    ))
    .bind(since_ms)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("away events since: list")?;
    Ok(rows.into_iter().map(away_row_to_record).collect())
}

/// `(total events, person events)` for the status document.
pub async fn away_stats(pool: &SqlitePool) -> Result<(i64, i64)> {
    let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM away_events")
        .fetch_one(pool)
        .await
        .context("away stats: total")?;
    let (visitors,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM away_events WHERE kind = 'person'")
            .fetch_one(pool)
            .await
            .context("away stats: visitors")?;
    Ok((total, visitors))
}

#[cfg(test)]
mod away_tests {
    use super::*;
    use crate::away::AwayEventRecord;

    fn rec(kind: &str, state: &str) -> AwayEventRecord {
        AwayEventRecord {
            id: 0,
            camera_id: "0".into(),
            kind: kind.into(),
            started_ms: 1_000,
            labels: "person×1".into(),
            face_name: None,
            description: None,
            visitor_reply: None,
            snapshot: Some(format!("{kind}.jpg")),
            state: state.into(),
        }
    }

    #[tokio::test]
    async fn insert_list_patch_prune_roundtrip() {
        let (pool, _auth) = crate::db::create_test_dbs().await;
        crate::db::run_migrations(&pool)
            .await
            .expect("migrations for test pool");

        for i in 0..5 {
            let (_, pruned) = insert_away_event_with_cap(&pool, &rec("person", "greeting"), 3)
                .await
                .unwrap();
            // Inside the cap nothing is orphaned yet.
            assert!(pruned.is_empty() || i >= 3, "prune only past the cap");
        }
        let listed = list_away_events(&pool, 10).await.unwrap();
        assert_eq!(listed.len(), 3, "FIFO cap keeps the newest 3");
        assert_eq!(listed[0].id, 5);

        // Async description + reply + terminal state, each returning the
        // full row for the SSE update.
        let updated = update_away_description(&pool, listed[0].id, "一名男子站在桌旁")
            .await
            .unwrap()
            .expect("row exists");
        assert_eq!(updated.description.as_deref(), Some("一名男子站在桌旁"));
        assert_eq!(updated.state, "greeting");

        let answered = update_away_reply(&pool, listed[0].id, "我是快递员")
            .await
            .unwrap()
            .expect("row exists");
        assert_eq!(answered.visitor_reply.as_deref(), Some("我是快递员"));
        assert_eq!(answered.state, "answered");

        let silent = update_away_state(&pool, listed[1].id, "silent")
            .await
            .unwrap()
            .expect("row exists");
        assert_eq!(silent.state, "silent");

        assert_eq!(get_away_event(&pool, 999).await.unwrap(), None);

        // Stats then clear-all returns names for unlinking.
        assert_eq!(away_stats(&pool).await.unwrap(), (3, 3));
        let (removed, names) = clear_away_events(&pool).await.unwrap();
        assert_eq!(removed, 3);
        assert_eq!(names.len(), 3);
        assert!(list_away_events(&pool, 10).await.unwrap().is_empty());
    }
}

// ---------------------------------------------------------------------------
// Voiceprint speaker profiles (SPEC appendix A notebook dialect #25)
// ---------------------------------------------------------------------------

/// One enrolled speaker profile row. The embeddings blob itself is decoded
/// separately via [`load_voice_speaker_embeddings`].
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct VoiceSpeakerRow {
    pub id: i64,
    pub name: String,
    pub dim: i64,
    pub count: i64,
    pub created_at: String,
}

/// Encode enrollment embeddings as the little-endian f32 blob stored in
/// `voice_speakers.embeddings` (`count` vectors of `dim`, concatenated).
pub(crate) fn embeddings_to_blob(embeddings: &[Vec<f32>]) -> Vec<u8> {
    let total: usize = embeddings.iter().map(|v| v.len()).sum();
    let mut blob = Vec::with_capacity(total * 4);
    for v in embeddings {
        for x in v {
            blob.extend_from_slice(&x.to_le_bytes());
        }
    }
    blob
}

/// Decode the blob back into vectors; `None` when the byte length
/// disagrees with the stored `dim`/`count` (a corrupt row).
pub(crate) fn blob_to_embeddings(blob: &[u8], dim: i64, count: i64) -> Option<Vec<Vec<f32>>> {
    if dim <= 0 || count <= 0 {
        return None;
    }
    let dim = dim as usize;
    let count = count as usize;
    if blob.len() != dim * count * 4 {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    for c in 0..count {
        let mut v = Vec::with_capacity(dim);
        for d in 0..dim {
            let off = (c * dim + d) * 4;
            let mut b = [0u8; 4];
            b.copy_from_slice(&blob[off..off + 4]);
            v.push(f32::from_le_bytes(b));
        }
        out.push(v);
    }
    Some(out)
}

/// Upsert one enrolled speaker (embeddings replaced wholesale on conflict).
pub async fn insert_voice_speaker(
    pool: &SqlitePool,
    name: &str,
    dim: i64,
    embeddings: &[Vec<f32>],
) -> Result<()> {
    let count = embeddings.len() as i64;
    let blob = embeddings_to_blob(embeddings);
    sqlx::query(
        "INSERT INTO voice_speakers (name, dim, count, embeddings) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(name) DO UPDATE SET dim = ?2, count = ?3, embeddings = ?4",
    )
    .bind(name)
    .bind(dim)
    .bind(count)
    .bind(&blob)
    .execute(pool)
    .await
    .context("voice speaker: upsert")?;
    Ok(())
}

/// Delete an enrolled speaker; returns whether the name existed.
pub async fn delete_voice_speaker(pool: &SqlitePool, name: &str) -> Result<bool> {
    let result = sqlx::query("DELETE FROM voice_speakers WHERE name = ?1")
        .bind(name)
        .execute(pool)
        .await
        .context("voice speaker: delete")?;
    Ok(result.rows_affected() > 0)
}

/// List enrolled speakers (metadata only).
/// Face enrollment row (SPEC appendix A #33) — the embedding BLOB is
/// LE f32, `dim` values.
#[derive(Debug, serde::Serialize)]
pub struct FaceRow {
    pub id: i64,
    pub name: String,
    pub dim: i64,
    pub created_at: String,
}

fn face_to_blob(v: &[f32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(v.len() * 4);
    for x in v {
        b.extend_from_slice(&x.to_le_bytes());
    }
    b
}

fn blob_to_face(b: &[u8], dim: i64) -> Vec<f32> {
    let n = (b.len() / 4).min(dim.max(0) as usize);
    b.as_chunks::<4>()
        .0
        .iter()
        .take(n)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

pub async fn insert_face(pool: &SqlitePool, name: &str, dim: i64, embedding: &[f32]) -> Result<()> {
    sqlx::query(
        "INSERT INTO faces (name, dim, embedding) VALUES (?1, ?2, ?3)
         ON CONFLICT(name) DO UPDATE SET dim = excluded.dim, embedding = excluded.embedding",
    )
    .bind(name)
    .bind(dim)
    .bind(face_to_blob(embedding))
    .execute(pool)
    .await
    .context("face: insert")?;
    Ok(())
}

pub async fn list_faces(pool: &SqlitePool) -> Result<Vec<FaceRow>> {
    let rows: Vec<(i64, String, i64, String)> =
        sqlx::query_as("SELECT id, name, dim, created_at FROM faces ORDER BY name")
            .fetch_all(pool)
            .await
            .context("face: list")?;
    Ok(rows
        .into_iter()
        .map(|(id, name, dim, created_at)| FaceRow {
            id,
            name,
            dim,
            created_at,
        })
        .collect())
}

/// All enrolled templates for the boot-time registry load.
pub async fn load_face_templates(pool: &SqlitePool) -> Result<Vec<streaming::face::EnrolledFace>> {
    let rows: Vec<(String, i64, Vec<u8>)> =
        sqlx::query_as("SELECT name, dim, embedding FROM faces ORDER BY name")
            .fetch_all(pool)
            .await
            .context("face: load templates")?;
    Ok(rows
        .into_iter()
        .map(|(name, dim, embedding)| streaming::face::EnrolledFace {
            name,
            embedding: blob_to_face(&embedding, dim),
        })
        .collect())
}

pub async fn delete_face(pool: &SqlitePool, name: &str) -> Result<bool> {
    let r = sqlx::query("DELETE FROM faces WHERE name = ?1")
        .bind(name)
        .execute(pool)
        .await
        .context("face: delete")?;
    Ok(r.rows_affected() > 0)
}

pub async fn list_voice_speakers(pool: &SqlitePool) -> Result<Vec<VoiceSpeakerRow>> {
    let rows: Vec<(i64, String, i64, i64, String)> =
        sqlx::query_as("SELECT id, name, dim, count, created_at FROM voice_speakers ORDER BY name")
            .fetch_all(pool)
            .await
            .context("voice speaker: list")?;
    Ok(rows
        .into_iter()
        .map(|(id, name, dim, count, created_at)| VoiceSpeakerRow {
            id,
            name,
            dim,
            count,
            created_at,
        })
        .collect())
}

/// Every profile's decoded embeddings, for the boot-time engine load.
/// Corrupt rows are skipped with a warning rather than failing the boot.
pub async fn load_voice_speaker_embeddings(
    pool: &SqlitePool,
) -> Result<Vec<(String, Vec<Vec<f32>>)>> {
    let rows: Vec<(String, i64, i64, Vec<u8>)> =
        sqlx::query_as("SELECT name, dim, count, embeddings FROM voice_speakers ORDER BY name")
            .fetch_all(pool)
            .await
            .context("voice speaker: load")?;
    let mut out = Vec::new();
    for (name, dim, count, blob) in rows {
        match blob_to_embeddings(&blob, dim, count) {
            Some(embeddings) => out.push((name, embeddings)),
            None => tracing::warn!(speaker = %name, "voice speaker: corrupt row skipped"),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Meetings (SPEC appendix A #27)
// ---------------------------------------------------------------------------

/// One meeting recording row.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow, serde::Serialize)]
pub struct MeetingRow {
    pub id: i64,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    /// `recording` | `processing` | `done` | `failed`.
    pub status: String,
    pub num_speakers: Option<i64>,
    pub num_segments: Option<i64>,
    pub audio_path: Option<String>,
    pub error: String,
}

/// One diarized + transcribed meeting segment.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MeetingSegmentRow {
    pub start_ms: i64,
    pub end_ms: i64,
    pub speaker_index: i64,
    /// Enrolled voiceprint name ("" = anonymous cluster).
    pub speaker: String,
    pub text: String,
}

/// Insert a new recording row and return its id.
pub async fn insert_meeting_started(pool: &SqlitePool, started_at_ms: i64) -> Result<i64> {
    let row: (i64,) =
        sqlx::query_as("INSERT INTO meeting_records (started_at_ms) VALUES (?1) RETURNING id")
            .bind(started_at_ms)
            .fetch_one(pool)
            .await
            .context("meeting: insert started")?;
    Ok(row.0)
}

/// Update the status (and error text) of a meeting row.
pub async fn update_meeting_status(
    pool: &SqlitePool,
    id: i64,
    status: &str,
    error: &str,
) -> Result<()> {
    sqlx::query("UPDATE meeting_records SET status = ?2, error = ?3 WHERE id = ?1")
        .bind(id)
        .bind(status)
        .bind(error)
        .execute(pool)
        .await
        .context("meeting: update status")?;
    Ok(())
}

/// Persist the finished pipeline result.
pub async fn finalize_meeting_done(
    pool: &SqlitePool,
    id: i64,
    ended_at_ms: i64,
    duration_ms: i64,
    num_speakers: i64,
    audio_path: &str,
    segments: &[streaming::meeting::TranscriptSegment],
) -> Result<()> {
    let mut tx = pool.begin().await.context("meeting: begin finalize tx")?;
    sqlx::query(
        "UPDATE meeting_records SET status = 'done', ended_at_ms = ?2, duration_ms = ?3,          num_speakers = ?4, num_segments = ?5, audio_path = ?6, error = '' WHERE id = ?1",
    )
    .bind(id)
    .bind(ended_at_ms)
    .bind(duration_ms)
    .bind(num_speakers)
    .bind(segments.len() as i64)
    .bind(audio_path)
    .execute(&mut *tx)
    .await
    .context("meeting: finalize row")?;
    for seg in segments {
        sqlx::query(
            "INSERT INTO meeting_segments              (meeting_id, start_ms, end_ms, speaker_index, speaker, text)              VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(id)
        .bind(seg.start_ms as i64)
        .bind(seg.end_ms as i64)
        .bind(i64::from(seg.speaker_index))
        .bind(&seg.speaker)
        .bind(&seg.text)
        .execute(&mut *tx)
        .await
        .context("meeting: insert segment")?;
    }
    tx.commit().await.context("meeting: commit finalize")?;
    Ok(())
}

/// List meetings newest-first.
pub async fn list_meetings(pool: &SqlitePool) -> Result<Vec<MeetingRow>> {
    let rows: Vec<MeetingRow> = sqlx::query_as(
        "SELECT id, started_at_ms, ended_at_ms, duration_ms, status, num_speakers,          num_segments, audio_path, error FROM meeting_records          ORDER BY started_at_ms DESC, id DESC",
    )
    .fetch_all(pool)
    .await
    .context("meeting: list")?;
    Ok(rows)
}

/// Fetch one meeting row.
pub async fn get_meeting(pool: &SqlitePool, id: i64) -> Result<Option<MeetingRow>> {
    let row: Option<MeetingRow> = sqlx::query_as(
        "SELECT id, started_at_ms, ended_at_ms, duration_ms, status, num_speakers,          num_segments, audio_path, error FROM meeting_records WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("meeting: get")?;
    Ok(row)
}

/// Fetch one meeting's segments ordered by start time.
pub async fn list_meeting_segments(
    pool: &SqlitePool,
    meeting_id: i64,
) -> Result<Vec<MeetingSegmentRow>> {
    let rows: Vec<(i64, i64, i64, String, String)> = sqlx::query_as(
        "SELECT start_ms, end_ms, speaker_index, speaker, text FROM meeting_segments          WHERE meeting_id = ?1 ORDER BY start_ms ASC, id ASC",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await
    .context("meeting: list segments")?;
    Ok(rows
        .into_iter()
        .map(
            |(start_ms, end_ms, speaker_index, speaker, text)| MeetingSegmentRow {
                start_ms,
                end_ms,
                speaker_index,
                speaker,
                text,
            },
        )
        .collect())
}

/// Delete a meeting, its segments, and any retained audio file.
/// Returns whether the meeting existed.
pub async fn delete_meeting(pool: &SqlitePool, id: i64) -> Result<bool> {
    let row = get_meeting(pool, id).await?;
    let Some(meeting) = row else {
        return Ok(false);
    };
    let mut tx = pool.begin().await.context("meeting: begin delete tx")?;
    sqlx::query("DELETE FROM meeting_segments WHERE meeting_id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("meeting: delete segments")?;
    sqlx::query("DELETE FROM meeting_records WHERE id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("meeting: delete row")?;
    tx.commit().await.context("meeting: commit delete")?;
    if let Some(path) = meeting.audio_path.filter(|p| !p.is_empty()) {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(error = %e, path = %path, "meeting: retained audio delete failed")
            }
        }
    }
    Ok(true)
}

/// Initialize the SQLite pool at `path` with WAL mode and busy timeout.
///
/// This creates an async SqlitePool for web CRUD operations.
pub async fn init_pool(path: &str) -> Result<SqlitePool> {
    let connect_options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .busy_timeout(std::time::Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(10)
        .connect_with(connect_options)
        .await
        .context("Failed to create SQLite pool")?;

    // Enable WAL mode and busy timeout
    sqlx::query("PRAGMA journal_mode=WAL;")
        .execute(&pool)
        .await
        .context("Failed to set WAL mode")?;

    sqlx::query("PRAGMA busy_timeout=5000;")
        .execute(&pool)
        .await
        .context("Failed to set busy timeout")?;

    run_migrations(&pool)
        .await
        .context("Failed to run database migrations")?;

    Ok(pool)
}

/// Initialize the blocking Connection for the security crate.
///
/// This is used for blocking rusqlite operations (auth, sessions).
pub fn init_auth_db(path: &str) -> Result<Connection> {
    let conn = Connection::open(path).context("Failed to open SQLite database for auth")?;

    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .context("Failed to set WAL mode")?;

    conn.execute_batch("PRAGMA busy_timeout=5000;")
        .context("Failed to set busy timeout")?;

    Ok(conn)
}

/// The migrations directory embedded at compile time — the complete,
/// self-contained migration set that ships inside the binary. Deployed
/// instances used to rely on a `migrations/` directory landing next to
/// the binary (WorkingDirectory); forgetting to copy a new file made the
/// new tables silently missing while inserts only WARNed (2026-10-06
/// incident with `conversation_turns`). The embedded set is now the base
/// and can never drift from the shipped code.
static EMBEDDED_MIGRATIONS: include_dir::Dir =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/../../migrations");

/// Site-local migrations dir overlay (first existing wins), letting a
/// deployment add or shadow migrations without rebuilding. Same search
/// order the disk-only implementation used:
///   1. `migrations/` relative to the current working directory,
///   2. the build-time path `{CARGO_MANIFEST_DIR}/../../migrations`,
///   3. `/usr/local/share/mibee-eye/migrations` (Dockerfile install).
fn local_migrations_dir() -> Option<std::path::PathBuf> {
    let candidates = [
        Path::new("migrations").to_path_buf(),
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .map(|p| p.join("migrations"))
            .unwrap_or_else(|| Path::new("migrations").to_path_buf()),
        Path::new("/usr/local/share/mibee-eye/migrations").to_path_buf(),
    ];
    candidates.into_iter().find(|p| p.is_dir())
}

/// Parse a migration file name into `(version, name)`: "012_foo.sql" →
/// `(12, "012_foo.sql")`. Non-conforming names are ignored.
fn migration_number(name: &str) -> Option<i32> {
    if !name.ends_with(".sql") {
        return None;
    }
    name.split('_').next()?.parse().ok()
}

/// The full migration set: embedded files overlaid by any site-local
/// dir entries, keyed by version number. Local files win on collision.
fn collect_migrations() -> Vec<(i32, String, String)> {
    use std::collections::BTreeMap;
    let mut set: BTreeMap<i32, (String, String)> = BTreeMap::new();
    for file in EMBEDDED_MIGRATIONS.files() {
        let name = file
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if let Some(num) = migration_number(name) {
            set.insert(
                num,
                (
                    name.to_string(),
                    file.contents_utf8().unwrap_or("").to_string(),
                ),
            );
        }
    }
    if let Some(dir) = local_migrations_dir()
        && let Ok(entries) = std::fs::read_dir(&dir)
    {
        for entry in entries.filter_map(std::result::Result::ok) {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(num) = migration_number(&name) else {
                continue;
            };
            if let Ok(sql) = std::fs::read_to_string(entry.path()) {
                set.insert(num, (name, sql));
            }
        }
    }
    set.into_iter()
        .map(|(num, (name, sql))| (num, name, sql))
        .collect()
}

/// Ensure the schema_version table exists, read current version, apply pending
/// migrations, and update the version.
pub(crate) async fn run_migrations(pool: &SqlitePool) -> Result<()> {
    // Create version tracking table if it doesn't exist
    sqlx::query("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);")
        .execute(pool)
        .await
        .context("Failed to create schema_version table")?;

    // Get current version (0 if none)
    let current_version: i32 =
        sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM schema_version")
            .fetch_one(pool)
            .await
            .unwrap_or(0);

    // Apply each migration that hasn't been applied yet
    for (num, name, sql) in collect_migrations() {
        if num <= current_version {
            continue;
        }

        sqlx::query(&sql)
            .execute(pool)
            .await
            .with_context(|| format!("Failed to apply migration {num} ({name})"))?;

        sqlx::query("INSERT INTO schema_version (version) VALUES (?1)")
            .bind(num)
            .execute(pool)
            .await
            .context("Failed to update schema_version")?;

        tracing::info!(migration = num, name = %name, "Applied database migration");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Camera CRUD
// ---------------------------------------------------------------------------

pub async fn create_camera(pool: &SqlitePool, camera: &CameraRow) -> Result<()> {
    let config_str = serde_json::to_string(&camera.config).context("Failed to serialize config")?;

    sqlx::query(
        "INSERT INTO cameras (id, name, camera_type, config, status, created_at, updated_at, offline_since)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(&camera.id)
    .bind(&camera.name)
    .bind(&camera.camera_type)
    .bind(&config_str)
    .bind(&camera.status)
    .bind(&camera.created_at)
    .bind(&camera.updated_at)
    .bind(&camera.offline_since)
    .execute(pool)
    .await
    .context("Failed to create camera")?;

    Ok(())
}

pub async fn get_camera(pool: &SqlitePool, id: &str) -> Result<Option<CameraRow>> {
    let row = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            Option<String>,
        ),
    >(
        "SELECT id, name, camera_type, config, status, created_at, updated_at, offline_since
         FROM cameras WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("Failed to get camera")?;

    match row {
        Some((
            id,
            name,
            camera_type,
            config_str,
            status,
            created_at,
            updated_at,
            offline_since,
        )) => {
            let config: serde_json::Value =
                serde_json::from_str(&config_str).context("Failed to parse camera config JSON")?;
            Ok(Some(CameraRow {
                id,
                name,
                camera_type,
                config,
                status,
                created_at,
                updated_at,
                offline_since,
            }))
        }
        None => Ok(None),
    }
}

pub async fn list_cameras(pool: &SqlitePool) -> Result<Vec<CameraRow>> {
    let rows = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            Option<String>,
        ),
    >(
        "SELECT id, name, camera_type, config, status, created_at, updated_at, offline_since
         FROM cameras ORDER BY created_at DESC",
    )
    .fetch_all(pool)
    .await
    .context("Failed to list cameras")?;

    let mut cameras = Vec::new();
    for (id, name, camera_type, config_str, status, created_at, updated_at, offline_since) in rows {
        let config: serde_json::Value =
            serde_json::from_str(&config_str).context("Failed to parse camera config JSON")?;
        cameras.push(CameraRow {
            id,
            name,
            camera_type,
            config,
            status,
            created_at,
            updated_at,
            offline_since,
        });
    }

    Ok(cameras)
}

/// Auto-discover physically attached video devices and create camera entries
/// for any that don't already exist in the database.
///
/// This runs on startup so the user sees their webcam in the dashboard
/// immediately without manual configuration.
pub async fn auto_discover_cameras(pool: &SqlitePool) -> Result<usize> {
    let devices = match capture::video::enumerate_devices() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "failed to enumerate video devices during auto-discovery");
            return Ok(0);
        }
    };

    let existing = list_cameras(pool).await?;
    let existing_indices: std::collections::HashSet<i64> = existing
        .iter()
        .filter(|c| c.camera_type == "usb")
        .filter_map(|c| c.config.get("device_index").and_then(|v| v.as_i64()))
        .collect();

    let mut discovered = 0;
    for dev in &devices {
        let idx = dev.index as i64;
        if existing_indices.contains(&idx) {
            continue; // already in DB
        }

        // Skip metadata-only device nodes (UVC cameras expose multiple /dev/videoN,
        // only one has actual capture formats).
        if dev.formats.is_empty() {
            tracing::debug!(
                index = dev.index,
                "skipping device with no formats (likely metadata node)"
            );
            continue;
        }

        let name = if dev.name.is_empty() {
            format!("USB Camera {}", dev.index)
        } else {
            dev.name.clone()
        };

        let now = chrono_epoch_secs();
        let camera = CameraRow {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            camera_type: "usb".to_string(),
            config: serde_json::json!({"device_index": dev.index}),
            status: "stopped".to_string(),
            created_at: now.clone(),
            updated_at: now,
            offline_since: None,
        };

        match create_camera(pool, &camera).await {
            Ok(()) => {
                tracing::info!(
                    index = dev.index,
                    name = %camera.name,
                    "auto-discovered camera"
                );
                discovered += 1;
            }
            Err(e) => {
                tracing::warn!(error = %e, index = dev.index, "failed to auto-discover camera");
            }
        }
    }

    Ok(discovered)
}

fn chrono_epoch_secs() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

/// Mark all USB cameras with the given device index as offline.
///
/// Called by the hot-plug monitor when udev detects device removal.
/// Sets `status = 'offline'` and `offline_since = datetime('now')`.
/// Returns the IDs of the affected cameras so the caller can stop
/// their active streams (if any) and broadcast SSE events.
pub async fn mark_cameras_offline_by_device_index(
    pool: &SqlitePool,
    device_index: i64,
) -> Result<Vec<String>> {
    // First fetch matching camera IDs so the caller can stop streams.
    let cameras = list_cameras(pool).await?;
    let affected: Vec<String> = cameras
        .into_iter()
        .filter(|c| {
            c.camera_type == "usb"
                && c.status != "offline"
                && c.config.get("device_index").and_then(|v| v.as_i64()) == Some(device_index)
        })
        .map(|c| c.id)
        .collect();

    if affected.is_empty() {
        return Ok(vec![]);
    }

    // Bulk-update via json_extract (SQLite ≥ 3.38) or config LIKE.
    // We already know the IDs, so update them individually for clarity.
    let now = chrono_epoch_secs();
    for id in &affected {
        sqlx::query(
            "UPDATE cameras SET status = 'offline', offline_since = ?1, updated_at = ?1
             WHERE id = ?2",
        )
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await
        .context("Failed to mark camera offline")?;
    }

    tracing::info!(
        device_index,
        count = affected.len(),
        "marked cameras offline"
    );
    Ok(affected)
}

/// Clear the offline flag for cameras with the given device index.
///
/// Called by the hot-plug monitor when a device is plugged back in.
/// Resets `status` to `'stopped'` (does NOT auto-start) and clears
/// `offline_since`. Returns the IDs of the re-activated cameras.
pub async fn clear_offline_by_device_index(
    pool: &SqlitePool,
    device_index: i64,
) -> Result<Vec<String>> {
    let cameras = list_cameras(pool).await?;
    let affected: Vec<String> = cameras
        .into_iter()
        .filter(|c| {
            c.camera_type == "usb"
                && c.status == "offline"
                && c.config.get("device_index").and_then(|v| v.as_i64()) == Some(device_index)
        })
        .map(|c| c.id)
        .collect();

    if affected.is_empty() {
        return Ok(vec![]);
    }

    let now = chrono_epoch_secs();
    for id in &affected {
        sqlx::query(
            "UPDATE cameras SET status = 'stopped', offline_since = NULL, updated_at = ?1
             WHERE id = ?2",
        )
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await
        .context("Failed to clear offline status")?;
    }

    tracing::info!(
        device_index,
        count = affected.len(),
        "cleared offline status"
    );
    Ok(affected)
}

pub async fn update_camera(pool: &SqlitePool, camera: &CameraRow) -> Result<()> {
    let config_str = serde_json::to_string(&camera.config).context("Failed to serialize config")?;

    let result = sqlx::query(
        "UPDATE cameras SET name = ?1, camera_type = ?2, config = ?3,
                status = ?4, updated_at = ?5, offline_since = ?6
         WHERE id = ?7",
    )
    .bind(&camera.name)
    .bind(&camera.camera_type)
    .bind(&config_str)
    .bind(&camera.status)
    .bind(&camera.updated_at)
    .bind(&camera.offline_since)
    .bind(&camera.id)
    .execute(pool)
    .await
    .context("Failed to update camera")?;

    if result.rows_affected() == 0 {
        anyhow::bail!("Camera with id '{}' not found", camera.id);
    }

    Ok(())
}

pub async fn delete_camera(pool: &SqlitePool, id: &str) -> Result<()> {
    let result = sqlx::query("DELETE FROM cameras WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await
        .context("Failed to delete camera")?;

    if result.rows_affected() == 0 {
        anyhow::bail!("Camera with id '{}' not found", id);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Settings CRUD
// ---------------------------------------------------------------------------

pub async fn get_setting(pool: &SqlitePool, key: &str) -> Result<Option<String>> {
    let value = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?1")
        .bind(key)
        .fetch_optional(pool)
        .await
        .context("Failed to get setting")?;

    Ok(value)
}

pub async fn set_setting(pool: &SqlitePool, key: &str, value: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, datetime('now'))
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await
    .context("Failed to set setting")?;

    Ok(())
}

/// List all settings as a vector of (key, value) pairs.
pub async fn list_settings(pool: &SqlitePool) -> Result<Vec<(String, String)>> {
    let rows =
        sqlx::query_as::<_, (String, String)>("SELECT key, value FROM settings ORDER BY key")
            .fetch_all(pool)
            .await
            .context("Failed to list settings")?;

    Ok(rows)
}

// ---------------------------------------------------------------------------
// Protocol configs (onvif / gb28181 / rtmp_push)
// ---------------------------------------------------------------------------

/// Retrieve one protocol's config as a JSON Value, or None if not stored.
pub async fn get_protocol_config(
    pool: &SqlitePool,
    key: &str,
) -> Result<Option<serde_json::Value>> {
    let value_str: Option<String> =
        sqlx::query_scalar("SELECT value FROM protocol_configs WHERE key = ?1")
            .bind(key)
            .fetch_optional(pool)
            .await
            .context("Failed to get protocol config")?;

    match value_str {
        Some(s) => {
            let v: serde_json::Value =
                serde_json::from_str(&s).context("Failed to parse protocol config JSON")?;
            Ok(Some(v))
        }
        None => Ok(None),
    }
}

/// Upsert one protocol config (replaces existing value, updates timestamp).
pub async fn set_protocol_config(
    pool: &SqlitePool,
    key: &str,
    value: &serde_json::Value,
) -> Result<()> {
    let s = serde_json::to_string(value).context("Failed to serialize protocol config")?;
    let sql = "INSERT INTO protocol_configs (key, value, updated_at) VALUES (?1, ?2, datetime('now')) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at";
    sqlx::query(sql)
        .bind(key)
        .bind(s)
        .execute(pool)
        .await
        .context("Failed to upsert protocol config")?;
    Ok(())
}

/// Return all stored protocol configs as (key, value) pairs, ordered by key.
pub async fn list_protocol_configs(pool: &SqlitePool) -> Result<Vec<(String, serde_json::Value)>> {
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT key, value FROM protocol_configs ORDER BY key",
    )
    .fetch_all(pool)
    .await
    .context("Failed to list protocol configs")?;

    let mut out = Vec::new();
    for (key, value_str) in rows {
        let value: serde_json::Value =
            serde_json::from_str(&value_str).context("Failed to parse protocol config JSON")?;
        out.push((key, value));
    }
    Ok(out)
}

/// Return true if the protocol_configs table has zero rows (used at startup
/// to decide whether to seed from config.toml).
pub async fn protocol_configs_is_empty(pool: &SqlitePool) -> Result<bool> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM protocol_configs")
        .fetch_one(pool)
        .await
        .context("Failed to check if protocol_configs is empty")?;
    Ok(count == 0)
}

// ---------------------------------------------------------------------------
// Stream Session logging
// ---------------------------------------------------------------------------

/// Insert a new stream session row for audit purposes.
/// The session starts now with default zero counters.
pub async fn insert_stream_session(
    pool: &SqlitePool,
    session_id: &str,
    camera_id: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO stream_sessions (id, camera_id) VALUES (?1, ?2)")
        .bind(session_id)
        .bind(camera_id)
        .execute(pool)
        .await
        .context("Failed to insert stream session")?;
    Ok(())
}

/// Finalize (end) the most recent open session for a camera.
/// Updates stats if provided, otherwise records 0s.
/// Returns the number of rows updated (0 or 1).
pub async fn finalize_stream_session(
    pool: &SqlitePool,
    camera_id: &str,
    bytes_received: i64,
    frames_received: i64,
    error_count: i64,
) -> Result<usize> {
    let result = sqlx::query(
        "UPDATE stream_sessions
         SET ended_at = datetime('now'),
             bytes_received = ?1,
             frames_received = ?2,
             error_count = ?3
         WHERE id = (
             SELECT id FROM stream_sessions
             WHERE camera_id = ?4 AND ended_at IS NULL
             ORDER BY started_at DESC LIMIT 1
         )",
    )
    .bind(bytes_received)
    .bind(frames_received)
    .bind(error_count)
    .bind(camera_id)
    .execute(pool)
    .await
    .context("Failed to finalize stream session")?;

    Ok(result.rows_affected() as usize)
}

// ---------------------------------------------------------------------------
// Cloud AI config (SPEC §4.10) — dedicated table, never the settings bag
// ---------------------------------------------------------------------------

/// The stored cloud config; defaults when the row is somehow missing.
pub async fn get_cloud_config(pool: &SqlitePool) -> Result<crate::cloud::CloudConfig> {
    let row = sqlx::query_as::<_, (String, String, String, String, i64, i64)>(
        "SELECT provider, api_key, chat_model, vision_model, fallback_local, timeout_secs
         FROM cloud_config WHERE id = 1",
    )
    .fetch_optional(pool)
    .await
    .context("Failed to read cloud config")?;
    Ok(match row {
        Some((provider, api_key, chat_model, vision_model, fallback, timeout)) => {
            crate::cloud::CloudConfig {
                provider,
                api_key,
                chat_model,
                vision_model,
                fallback_local: fallback != 0,
                timeout_secs: timeout.max(5) as u64,
            }
        }
        None => crate::cloud::CloudConfig::default(),
    })
}

/// Full-row upsert (the PUT handler merged + validated beforehand).
pub async fn save_cloud_config(pool: &SqlitePool, cfg: &crate::cloud::CloudConfig) -> Result<()> {
    sqlx::query(
        "INSERT INTO cloud_config (id, provider, api_key, chat_model, vision_model, fallback_local, timeout_secs)
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET provider=?1, api_key=?2, chat_model=?3,
             vision_model=?4, fallback_local=?5, timeout_secs=?6",
    )
    .bind(&cfg.provider)
    .bind(&cfg.api_key)
    .bind(&cfg.chat_model)
    .bind(&cfg.vision_model)
    .bind(i64::from(cfg.fallback_local))
    .bind(cfg.timeout_secs as i64)
    .execute(pool)
    .await
    .context("Failed to save cloud config")?;
    Ok(())
}

#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use tokio::sync::Mutex;

/// Test helper: creates in-memory SqlitePool and auth_db for testing.
/// Returns (pool, auth_db) where pool is for web CRUD and auth_db is for
/// security operations.
#[cfg(test)]
pub async fn create_test_dbs() -> (SqlitePool, Arc<Mutex<rusqlite::Connection>>) {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(":memory:")
        .await
        .expect("Failed to create test pool");

    // Create in-memory connection for auth operations. The users/sessions
    // schema (migration 003) lives on this connection — apply it here so
    // every consumer gets a usable auth DB (single source of truth).
    let auth_conn = rusqlite::Connection::open_in_memory().expect("Failed to create test auth db");
    auth_conn
        .execute_batch(include_str!("../../../migrations/003_users_sessions.sql"))
        .expect("Failed to apply auth schema (003) to test auth db");
    let auth_db = Arc::new(Mutex::new(auth_conn));

    (pool, auth_db)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Create an in-memory database pool for testing.
    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .unwrap();

        // Use the real migration path: creates schema_version and applies
        // 001..004 in order, exactly like the running application.
        run_migrations(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn test_migration_tables_exist() {
        let pool = test_pool().await;

        // Verify all tables were created
        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();

        assert!(
            tables.contains(&"cameras".to_string()),
            "cameras table should exist"
        );
        assert!(
            tables.contains(&"settings".to_string()),
            "settings table should exist"
        );
        assert!(
            tables.contains(&"stream_sessions".to_string()),
            "stream_sessions table should exist"
        );
    }

    #[tokio::test]
    async fn test_camera_crud_roundtrip() {
        let pool = test_pool().await;

        let camera = CameraRow {
            id: "cam-001".to_string(),
            name: "Front Door".to_string(),
            camera_type: "rtsp".to_string(),
            config: serde_json::json!({"url": "rtsp://192.168.1.100:554/stream1"}),
            status: "stopped".to_string(),
            created_at: "2025-01-01T00:00:00".to_string(),
            updated_at: "2025-01-01T00:00:00".to_string(),
            offline_since: None,
        };

        // Create
        create_camera(&pool, &camera).await.unwrap();

        // Read
        let fetched = get_camera(&pool, "cam-001")
            .await
            .unwrap()
            .expect("Camera should exist");
        assert_eq!(fetched.name, "Front Door");
        assert_eq!(fetched.camera_type, "rtsp");
        assert_eq!(fetched.config["url"], "rtsp://192.168.1.100:554/stream1");

        // Update
        let mut updated = camera.clone();
        updated.name = "Back Door".to_string();
        updated.status = "running".to_string();
        updated.updated_at = "2025-01-02T00:00:00".to_string();
        update_camera(&pool, &updated).await.unwrap();

        let fetched = get_camera(&pool, "cam-001")
            .await
            .unwrap()
            .expect("Camera should exist after update");
        assert_eq!(fetched.name, "Back Door");
        assert_eq!(fetched.status, "running");

        // List
        let all = list_cameras(&pool).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "cam-001");

        // Delete
        delete_camera(&pool, "cam-001").await.unwrap();
        let fetched = get_camera(&pool, "cam-001").await.unwrap();
        assert!(fetched.is_none(), "Camera should be deleted");

        // List empty
        let all = list_cameras(&pool).await.unwrap();
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn test_settings_get_set() {
        let pool = test_pool().await;

        // Get non-existent key
        let val = get_setting(&pool, "nonexistent").await.unwrap();
        assert!(val.is_none());

        // Set and get
        set_setting(&pool, "theme", "dark").await.unwrap();
        let val = get_setting(&pool, "theme").await.unwrap();
        assert_eq!(val, Some("dark".to_string()));

        // Update
        set_setting(&pool, "theme", "light").await.unwrap();
        let val = get_setting(&pool, "theme").await.unwrap();
        assert_eq!(val, Some("light".to_string()));

        // Multiple settings
        set_setting(&pool, "language", "zh-CN").await.unwrap();
        let val = get_setting(&pool, "language").await.unwrap();
        assert_eq!(val, Some("zh-CN".to_string()));

        // Original still intact
        let val = get_setting(&pool, "theme").await.unwrap();
        assert_eq!(val, Some("light".to_string()));
    }

    #[tokio::test]
    async fn test_delete_nonexistent_camera_fails() {
        let pool = test_pool().await;
        let result = delete_camera(&pool, "no-such-camera").await;
        assert!(result.is_err(), "Deleting nonexistent camera should fail");
    }

    #[tokio::test]
    async fn test_update_nonexistent_camera_fails() {
        let pool = test_pool().await;
        let camera = CameraRow {
            id: "no-such".to_string(),
            name: "Ghost".to_string(),
            camera_type: "rtsp".to_string(),
            config: serde_json::Value::Object(Default::default()),
            status: "stopped".to_string(),
            created_at: "".to_string(),
            updated_at: "".to_string(),
            offline_since: None,
        };
        let result = update_camera(&pool, &camera).await;
        assert!(result.is_err(), "Updating nonexistent camera should fail");
    }

    #[tokio::test]
    async fn test_cameras_have_type_index() {
        let pool = test_pool().await;
        let indexes: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='cameras'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert!(
            indexes.contains(&"idx_cameras_type".to_string()),
            "idx_cameras_type index should exist"
        );
    }

    // -----------------------------------------------------------------------
    // Stream Session tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_insert_and_finalize_stream_session() {
        let pool = test_pool().await;

        // Create a camera first (FK constraint)
        sqlx::query(concat!(
            "INSERT INTO cameras (id, name, camera_type, config, status, created_at, updated_at) ",
            "VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        ))
        .bind("cam-001")
        .bind("Front Door")
        .bind("usb")
        .bind("{}")
        .bind("stopped")
        .bind("now")
        .bind("now")
        .execute(&pool)
        .await
        .unwrap();

        // Insert a stream session
        insert_stream_session(&pool, "sess-001", "cam-001")
            .await
            .unwrap();

        // Verify the row exists with ended_at IS NULL
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM stream_sessions
             WHERE camera_id = ?1 AND ended_at IS NULL",
        )
        .bind("cam-001")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1, "should have one open session");

        // Finalize the session with stats
        let updated = finalize_stream_session(&pool, "cam-001", 1024, 30, 0)
            .await
            .unwrap();
        assert_eq!(updated, 1, "should update exactly one row");

        // Verify the row now has ended_at NOT NULL and stats recorded
        let (ended_count, bytes, frames, errors): (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), bytes_received, frames_received, error_count
             FROM stream_sessions
             WHERE camera_id = ?1 AND ended_at IS NOT NULL",
        )
        .bind("cam-001")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ended_count, 1, "should have one ended session");
        assert_eq!(bytes, 1024);
        assert_eq!(frames, 30);
        assert_eq!(errors, 0);
    }

    #[tokio::test]
    async fn test_insert_stream_session_uses_defaults() {
        let pool = test_pool().await;

        // Create a camera first (FK constraint)
        sqlx::query(concat!(
            "INSERT INTO cameras (id, name, camera_type, config, status, created_at, updated_at) ",
            "VALUES ('cam-002', 'Test', 'usb', '{}', 'stopped', datetime('now'), datetime('now'));",
        ))
        .execute(&pool)
        .await
        .unwrap();

        insert_stream_session(&pool, "sess-002", "cam-002")
            .await
            .unwrap();

        // Verify default values for numeric columns
        let (bytes, frames, errors): (i64, i64, i64) = sqlx::query_as(
            "SELECT bytes_received, frames_received, error_count
             FROM stream_sessions WHERE id = ?1",
        )
        .bind("sess-002")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(bytes, 0, "bytes_received should default to 0");
        assert_eq!(frames, 0, "frames_received should default to 0");
        assert_eq!(errors, 0, "error_count should default to 0");
    }

    #[tokio::test]
    async fn test_finalize_only_latest_open_session() {
        let pool = test_pool().await;

        // Create a camera first (FK constraint)
        sqlx::query(concat!(
            "INSERT INTO cameras (id, name, camera_type, config, status, created_at, updated_at) ",
            "VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'), datetime('now'))",
        ))
        .bind("cam-001")
        .bind("Front Door")
        .bind("usb")
        .bind("{}")
        .bind("stopped")
        .execute(&pool)
        .await
        .unwrap();

        // Insert two open sessions with explicit timestamps to ensure ordering
        sqlx::query(concat!(
            "INSERT INTO stream_sessions (id, camera_id, started_at) ",
            "VALUES (?1, ?2, ?3)",
        ))
        .bind("sess-a")
        .bind("cam-001")
        .bind("2025-01-01T00:00:00")
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(concat!(
            "INSERT INTO stream_sessions (id, camera_id, started_at) ",
            "VALUES (?1, ?2, ?3)",
        ))
        .bind("sess-b")
        .bind("cam-001")
        .bind("2025-01-02T00:00:00")
        .execute(&pool)
        .await
        .unwrap();

        // Finalize — should only affect the latest (sess-b)
        let updated = finalize_stream_session(&pool, "cam-001", 500, 10, 1)
            .await
            .unwrap();
        assert_eq!(updated, 1, "should update exactly one row");

        // sess-a should still be open
        let open_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM stream_sessions
             WHERE camera_id = ?1 AND ended_at IS NULL",
        )
        .bind("cam-001")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(open_count, 1, "sess-a should still be open");

        // The finalized one should have the stats
        let (sess_b_ended, bytes, frames, errors): (bool, i64, i64, i64) = sqlx::query_as(
            "SELECT ended_at IS NOT NULL, bytes_received, frames_received, error_count
             FROM stream_sessions WHERE id = ?1",
        )
        .bind("sess-b")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(sess_b_ended, "sess-b should be ended");
        assert_eq!(bytes, 500);
        assert_eq!(frames, 10);
        assert_eq!(errors, 1);
    }

    #[tokio::test]
    async fn hearing_records_table_created_by_migration() {
        let pool = test_pool().await;
        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(
            tables.contains(&"hearing_records".to_string()),
            "hearing_records table should exist"
        );
    }

    #[tokio::test]
    async fn hearing_record_insert_and_list_roundtrip() {
        let pool = test_pool().await;
        insert_hearing_record(&pool, "sound", "Dog", Some(0.62), "", "", "", "", 1_000)
            .await
            .unwrap();
        insert_hearing_record(
            &pool,
            "voice",
            "今天天气怎么样",
            None,
            "小蜜蜂",
            "mickey",
            "实时检测：1×person（中间）",
            "recordings/cam_20261001.mp4",
            2_000,
        )
        .await
        .unwrap();

        let rows = list_hearing_records(&pool, 100, None).await.unwrap();
        assert_eq!(rows.len(), 2, "both records listed");
        // Newest first.
        assert_eq!(rows[0].kind, "voice");
        assert_eq!(rows[0].text, "今天天气怎么样");
        assert_eq!(rows[0].keyword, "小蜜蜂");
        assert_eq!(rows[0].speaker, "mickey");
        assert_eq!(rows[0].score, None);
        // Correlated-record dimensions round-trip (#30-C).
        assert_eq!(rows[0].scene, "实时检测：1×person（中间）");
        assert_eq!(rows[0].media_ref, "recordings/cam_20261001.mp4");
        assert_eq!(rows[1].kind, "sound");
        assert_eq!(rows[1].text, "Dog");
        assert_eq!(rows[1].score, Some(0.62));
        assert_eq!(rows[1].keyword, "");
        assert_eq!(rows[1].speaker, "", "sound records carry no speaker");
    }

    #[tokio::test]
    async fn hearing_record_kind_filter_and_limit() {
        let pool = test_pool().await;
        for i in 0..5 {
            insert_hearing_record(&pool, "sound", "Dog", Some(0.5), "", "", "", "", i * 10)
                .await
                .unwrap();
            insert_hearing_record(
                &pool,
                "voice",
                "你好",
                None,
                "小蜜蜂",
                "",
                "",
                "",
                i * 10 + 5,
            )
            .await
            .unwrap();
        }

        let sounds = list_hearing_records(&pool, 100, Some("sound"))
            .await
            .unwrap();
        assert_eq!(sounds.len(), 5);
        assert!(sounds.iter().all(|r| r.kind == "sound"));

        let voices = list_hearing_records(&pool, 2, Some("voice")).await.unwrap();
        assert_eq!(voices.len(), 2, "limit applies within the filter");
        assert_eq!(voices[0].timestamp_ms, 45, "newest voice record first");

        // Unknown kind values are treated as "no filter", not an error.
        let all = list_hearing_records(&pool, 100, Some("bogus"))
            .await
            .unwrap();
        assert_eq!(all.len(), 10);
    }

    #[tokio::test]
    async fn hearing_record_fifo_cap_prunes_oldest() {
        let pool = test_pool().await;
        for i in 0..(HEARING_RECORDS_CAP + 50) {
            insert_hearing_record(&pool, "sound", "Knock", Some(0.4), "", "", "", "", i)
                .await
                .unwrap();
        }
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM hearing_records")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, HEARING_RECORDS_CAP, "table stays at the FIFO cap");
        let oldest: i64 = sqlx::query_scalar("SELECT MIN(timestamp_ms) FROM hearing_records")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(oldest, 50, "the oldest 50 rows were pruned");
    }

    #[tokio::test]
    async fn hearing_record_clear_removes_everything() {
        let pool = test_pool().await;
        insert_hearing_record(&pool, "sound", "Glass", Some(0.9), "", "", "", "", 7)
            .await
            .unwrap();
        insert_hearing_record(&pool, "voice", "在吗", None, "小蜜蜂", "", "", "", 8)
            .await
            .unwrap();
        let removed = clear_hearing_records(&pool).await.unwrap();
        assert_eq!(removed, 2);
        let rows = list_hearing_records(&pool, 100, None).await.unwrap();
        assert!(rows.is_empty());
        // Clearing an empty table is a harmless 0.
        let removed = clear_hearing_records(&pool).await.unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn voice_speaker_blob_roundtrip_and_shape_guards() {
        let embeddings = vec![vec![0.25, -1.5, 3.0], vec![1.0, 2.0, 3.0]];
        let blob = embeddings_to_blob(&embeddings);
        assert_eq!(blob.len(), 3 * 2 * 4, "3 floats x 2 vectors x 4 bytes");
        assert_eq!(
            blob_to_embeddings(&blob, 3, 2).as_deref(),
            Some(&embeddings[..])
        );
        // Wrong byte length never panics — None instead. (2×3 is the
        // same total length, so it legitimately reshapes.)
        assert_eq!(blob_to_embeddings(&blob, 4, 2), None);
        assert_eq!(blob_to_embeddings(&blob, 0, 2), None);
        assert_eq!(blob_to_embeddings(&blob, 3, -1), None);
        assert_eq!(blob_to_embeddings(&[], 3, 0), None);
    }

    #[tokio::test]
    async fn voice_speaker_crud_roundtrip() {
        let pool = test_pool().await;
        let emb = vec![vec![0.5; 192], vec![0.6; 192]];
        insert_voice_speaker(&pool, "mickey", 192, &emb)
            .await
            .unwrap();

        let rows = list_voice_speakers(&pool).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "mickey");
        assert_eq!(rows[0].dim, 192);
        assert_eq!(rows[0].count, 2);

        let loaded = load_voice_speaker_embeddings(&pool).await.unwrap();
        assert_eq!(loaded, vec![("mickey".to_string(), emb.clone())]);

        // Upsert replaces wholesale.
        let emb2 = vec![vec![0.7; 192]];
        insert_voice_speaker(&pool, "mickey", 192, &emb2)
            .await
            .unwrap();
        let loaded = load_voice_speaker_embeddings(&pool).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].1.len(), 1, "re-enrollment replaces samples");

        // Second speaker sorts alongside; delete only removes the named one.
        insert_voice_speaker(&pool, "alice", 192, &emb)
            .await
            .unwrap();
        assert_eq!(list_voice_speakers(&pool).await.unwrap().len(), 2);
        assert!(delete_voice_speaker(&pool, "alice").await.unwrap());
        assert!(!delete_voice_speaker(&pool, "alice").await.unwrap());
        assert_eq!(list_voice_speakers(&pool).await.unwrap().len(), 1);

        // A corrupt row is skipped, not fatal.
        sqlx::query("UPDATE voice_speakers SET dim = 7 WHERE name = 'mickey'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            load_voice_speaker_embeddings(&pool)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn cloud_config_roundtrips_through_the_dedicated_table() {
        let (pool, _auth) = create_test_dbs().await;
        run_migrations(&pool).await.expect("migrations");
        let base = get_cloud_config(&pool).await.unwrap();
        assert_eq!(base.provider, "off");
        assert!(base.api_key.is_empty());
        let mut cfg = base.clone();
        cfg.provider = "openrouter".into();
        cfg.api_key = "sk-or-test".into();
        cfg.chat_model = "openai/gpt-4o-mini".into();
        cfg.vision_model = "qwen/qwen3-vl-8b".into();
        cfg.fallback_local = false;
        cfg.timeout_secs = 90;
        save_cloud_config(&pool, &cfg).await.unwrap();
        let back = get_cloud_config(&pool).await.unwrap();
        assert_eq!(back, cfg);
        // The key never lands in the settings bag.
        let rows = list_settings(&pool).await.unwrap();
        assert!(
            rows.iter()
                .all(|(k, v)| !k.starts_with("cloud") && !v.contains("sk-or-test"))
        );
    }

    #[test]
    fn migration_number_parses_names() {
        assert_eq!(migration_number("001_initial.sql"), Some(1));
        assert_eq!(migration_number("012_conversations.sql"), Some(12));
        assert_eq!(migration_number("notes.txt"), None);
        assert_eq!(migration_number("garbage.sql"), None);
    }

    /// The compile-time embedded set must cover every .sql file in the
    /// repo's migrations/ dir — a misconfigured include path would
    /// otherwise silently ship an incomplete schema (the 2026-10-06
    /// deployed-missing-migration incident, made impossible).
    #[test]
    fn embedded_migrations_cover_the_repo_set() {
        let repo_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations");
        let expected: std::collections::BTreeSet<String> = std::fs::read_dir(&repo_dir)
            .expect("repo migrations dir")
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".sql"))
            .collect();
        assert!(!expected.is_empty());
        let embedded: std::collections::BTreeSet<String> = EMBEDDED_MIGRATIONS
            .files()
            .map(|f| {
                f.path()
                    .file_name()
                    .expect("file name")
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        let missing: Vec<_> = expected.difference(&embedded).collect();
        assert!(missing.is_empty(), "missing from embedded set: {missing:?}");
        assert!(embedded.contains("012_conversations.sql"));
    }
}
