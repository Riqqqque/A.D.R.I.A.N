//! Local alarm replicas on satellites.
//!
//! Every scheduled alarm routed to a satellite endpoint is mirrored to that
//! satellite as an [`proto::AlarmReplica`]. The server stays the owner of
//! delivery: the satellite rings a replica itself only when it cannot reach
//! the server at due time, or when the server has not delivered it within a
//! grace window. Afterwards it reports the firing, and the delivery ledger
//! records that it rang locally instead of a failure.
//!
//! A late server delivery carries the alarm reference in `Speak`, so a
//! satellite that already rang that occurrence acks without ringing again.
//!
//! The replica set is pushed whole (`AlarmReplicaSync`) on connect and after
//! every alarm change, so a satellite that missed updates while offline
//! cannot keep a stale or canceled alarm.

use super::{ALARM_COLS, AlarmRow, map_alarm, next_occurrence_after};
use crate::db::P;
use crate::error::ApiError;
use crate::state::AppState;
use crate::util;
use adrian_protocol as proto;
use rusqlite::OptionalExtension;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// Sound the satellite synthesizes locally for a replica.
pub const REPLICA_SOUND: &str = "builtin:tone";

/// Scheduled alarms routed to `satellite_id`'s enabled endpoints.
pub async fn replicas_for(
    state: &AppState,
    satellite_id: &str,
) -> Result<Vec<proto::AlarmReplica>, ApiError> {
    state
        .db
        .q(
            "SELECT a.id, a.next_due_utc, a.volume, a.label, a.kind, a.schedule_version
             FROM alarms a JOIN endpoints e ON e.id = a.endpoint_id
             WHERE a.status = 'scheduled' AND a.deleted_at IS NULL
               AND a.next_due_utc IS NOT NULL
               AND e.kind = 'satellite' AND e.enabled = 1
               AND json_extract(e.config, '$.satellite_id') = ?1
             ORDER BY a.next_due_utc",
            vec![P::T(satellite_id.to_string())],
            |r| {
                let label: String = r.get(3)?;
                let kind: String = r.get(4)?;
                Ok(proto::AlarmReplica {
                    alarm_id: r.get(0)?,
                    occurrence_unix: r.get(1)?,
                    sound_id: REPLICA_SOUND.to_string(),
                    volume: r.get::<_, f64>(2)? as f32,
                    label: if label.is_empty() {
                        if kind == "timer" { "Timer" } else { "Alarm" }.to_string()
                    } else {
                        label
                    },
                    schedule_version: r.get::<_, i64>(5)?.max(0) as u64,
                })
            },
        )
        .await
}

/// Push the full replica set to one satellite, if it is connected.
pub async fn sync(state: &AppState, satellite_id: &str) {
    if !state.satellites.connected(satellite_id) {
        return;
    }
    match replicas_for(state, satellite_id).await {
        Ok(alarms) => {
            let env = proto::Envelope::new(proto::Message::AlarmReplicaSync { alarms });
            let _ = state.satellites.send(satellite_id, &env);
        }
        Err(e) => tracing::warn!(sat = %satellite_id, error = %e, "replica sync failed"),
    }
}

pub async fn sync_all(state: &AppState) {
    for id in state.satellites.connected_ids() {
        sync(state, &id).await;
    }
}

/// Resync connected satellites after alarm or endpoint changes. Bursts of
/// events are coalesced; a lagged receiver just triggers a full resync.
pub fn spawn_sync_on_changes(state: Arc<AppState>) {
    let mut rx = state.events.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) if ev.kind != "alarm" && ev.kind != "endpoint" => continue,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            while rx.try_recv().is_ok() {}
            sync_all(&state).await;
        }
    });
}

/// Satellite id behind an alarm's endpoint, when that endpoint is a
/// satellite.
async fn alarm_satellite(state: &AppState, alarm: &AlarmRow) -> Option<String> {
    let ep = alarm.endpoint_id.clone()?;
    state
        .db
        .q1(
            "SELECT json_extract(config, '$.satellite_id') FROM endpoints
             WHERE id = ?1 AND kind = 'satellite'",
            vec![P::T(ep)],
            |r| r.get::<_, Option<String>>(0),
        )
        .await
        .ok()
        .flatten()
        .flatten()
}

/// Apply a satellite's report of locally rung alarms. Only alarms routed to
/// that satellite are accepted, and only for an occurrence the server knows
/// about (an existing delivery row or the alarm's current due time).
/// Returns how many firings were recorded.
pub async fn record_local_firings(
    state: &AppState,
    satellite_id: &str,
    satellite_name: &str,
    fired: &[proto::LocalAlarmFiring],
) -> Result<usize, ApiError> {
    let mut recorded = 0;
    for f in fired {
        let alarm = state
            .db
            .q1(
                &format!("SELECT {ALARM_COLS} FROM alarms WHERE id = ?1"),
                vec![P::T(f.alarm_id.clone())],
                map_alarm,
            )
            .await?;
        let Some(alarm) = alarm else { continue };
        if alarm_satellite(state, &alarm).await.as_deref() != Some(satellite_id) {
            tracing::warn!(sat = %satellite_id, alarm = %f.alarm_id, "local firing for an alarm not routed to this satellite ignored");
            continue;
        }
        let fired_local = jiff::Timestamp::from_second(f.fired_at_unix)
            .map(|t| t.to_string())
            .unwrap_or_else(|_| f.fired_at_unix.to_string());
        let detail = format!(
            "rang locally on satellite '{satellite_name}' at {fired_local} from its alarm replica (server delivery unavailable; audibility not confirmed)"
        );
        let now = util::now_unix();
        let next = next_occurrence_after(&alarm, now);
        let alarm_c = alarm.clone();
        let occurrence = f.occurrence_unix;
        let applied = state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                // Server already tried this occurrence: correct its outcome.
                let existing: Option<String> = tx
                    .query_row(
                        "SELECT status FROM alarm_deliveries
                         WHERE alarm_id = ?1 AND occurrence_utc = ?2",
                        rusqlite::params![alarm_c.id, occurrence],
                        |r| r.get(0),
                    )
                    .optional()?;
                let applied = match existing.as_deref() {
                    Some("played") | Some("canceled") => false,
                    Some(_) => {
                        tx.execute(
                            "UPDATE alarm_deliveries SET status = 'played', detail = ?1,
                                finished_at = ?2
                             WHERE alarm_id = ?3 AND occurrence_utc = ?4",
                            rusqlite::params![detail, now, alarm_c.id, occurrence],
                        )?;
                        true
                    }
                    // Server never got to it (it was down): record the
                    // delivery and advance the alarm so it is not rung again.
                    None if alarm_c.status == "scheduled"
                        && alarm_c.next_due_utc == Some(occurrence) =>
                    {
                        tx.execute(
                            "INSERT INTO alarm_deliveries(id,alarm_id,occurrence_utc,execution_id,
                                endpoint_id,status,detail,started_at,finished_at)
                             VALUES(?1,?2,?3,?4,?5,'played',?6,?7,?7)",
                            rusqlite::params![
                                util::new_id(),
                                alarm_c.id,
                                occurrence,
                                util::new_id(),
                                alarm_c.endpoint_id,
                                detail,
                                now
                            ],
                        )?;
                        let status = if next.is_some() { "scheduled" } else { "done" };
                        tx.execute(
                            "UPDATE alarms SET status = ?1, next_due_utc = ?2, updated_at = ?3,
                                schedule_version = schedule_version + 1
                             WHERE id = ?4",
                            rusqlite::params![status, next, now, alarm_c.id],
                        )?;
                        true
                    }
                    None => false,
                };
                tx.commit()?;
                Ok(applied)
            })
            .await?;
        if applied {
            recorded += 1;
            state.emit(
                "alarm",
                json!({
                    "event": "delivered",
                    "alarm_id": alarm.id,
                    "status": "played",
                    "detail": "rang locally on satellite",
                    "occurrence_utc": occurrence,
                }),
            );
        }
    }
    Ok(recorded)
}
