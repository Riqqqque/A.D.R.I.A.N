//! Deterministic durable scheduler. The model may interpret a sentence, but
//! this module owns the alarm: commit first, acknowledge after; deduplicated
//! delivery records; restart reconciliation. No promise of universal
//! exactly-once external effects — outcomes are recorded honestly.

use crate::audio::AudioCommand;
use crate::db::{Db, P};
use crate::error::ApiError;
use crate::state::AppState;
use crate::util;
use jiff::tz::TimeZone;
use serde_json::{Value, json};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

pub mod replicas;

#[derive(Clone)]
pub struct SchedulerHandle {
    notify: Arc<Notify>,
}

impl SchedulerHandle {
    /// Handle without a running executor — `start` launches it once the
    /// shared state exists.
    pub fn detached() -> Self {
        Self {
            notify: Arc::new(Notify::new()),
        }
    }

    pub fn wake(&self) {
        self.notify.notify_one();
    }

    pub fn start(self, state: Arc<AppState>) {
        tokio::spawn(executor(state, self.notify.clone()));
    }
}

#[derive(Debug, Clone)]
pub struct AlarmRow {
    pub id: String,
    pub owner_id: i64,
    pub label: String,
    pub kind: String,
    pub timezone: String,
    pub local_time: Option<String>,
    pub recurrence: String,
    pub next_due_utc: Option<i64>,
    pub endpoint_id: Option<String>,
    pub volume: f64,
    pub status: String,
    pub snooze_minutes: i64,
}

fn map_alarm(r: &rusqlite::Row<'_>) -> rusqlite::Result<AlarmRow> {
    Ok(AlarmRow {
        id: r.get(0)?,
        owner_id: r.get(1)?,
        label: r.get(2)?,
        kind: r.get(3)?,
        timezone: r.get(4)?,
        local_time: r.get(5)?,
        recurrence: r.get(6)?,
        next_due_utc: r.get(7)?,
        endpoint_id: r.get(8)?,
        volume: r.get(9)?,
        status: r.get(10)?,
        snooze_minutes: r.get(11)?,
    })
}

const ALARM_COLS: &str = "id,owner_id,label,kind,timezone,local_time,recurrence,next_due_utc,endpoint_id,volume,status,snooze_minutes";

// ---------------------------------------------------------------------------
// Time resolution (jiff). Documented policy:
//   * nonexistent local times (spring-forward gap) -> shifted forward
//   * ambiguous local times (fall-back fold)      -> earlier occurrence
//   * elapsed timers (in_minutes) are civil-time independent
// ---------------------------------------------------------------------------

pub struct Resolved {
    pub due_utc: i64,
    pub local_display: String,
    pub local_time: Option<String>,
    pub notes: Vec<String>,
}

fn civil_to_zoned(tz: &TimeZone, dt: jiff::civil::DateTime) -> jiff::Zoned {
    // `compatible`: gap -> forward, fold -> earlier.
    dt.to_zoned(tz.clone()).unwrap_or_else(|_| {
        tz.to_ambiguous_zoned(dt)
            .compatible()
            .expect("compatible disambiguation always succeeds")
    })
}

fn parse_local(s: &str) -> Result<jiff::civil::DateTime, ApiError> {
    let norm = s.trim().replace(' ', "T");
    if norm.len() <= 5 {
        return Err(ApiError::BadRequest(
            "time-only value needs a date context".into(),
        ));
    }
    jiff::civil::DateTime::from_str(&norm).map_err(|_| {
        ApiError::BadRequest(format!(
            "cannot parse local time '{s}' — use 'YYYY-MM-DD HH:MM'"
        ))
    })
}

/// Next occurrence of `HH:MM` in `tz` strictly after `after`, honoring
/// recurrence (none/daily/weekdays).
fn next_clock_time(
    tz: &TimeZone,
    hhmm: &str,
    recurrence: &str,
    after: jiff::Timestamp,
) -> Result<jiff::Zoned, ApiError> {
    let mut it = hhmm.split(':');
    let (h, m) = (
        it.next().and_then(|v| v.parse::<i8>().ok()),
        it.next().and_then(|v| v.parse::<i8>().ok()),
    );
    let (Some(h), Some(m)) = (h, m) else {
        return Err(ApiError::BadRequest(format!("bad HH:MM value '{hhmm}'")));
    };
    if !(0..24).contains(&h) || !(0..60).contains(&m) {
        return Err(ApiError::BadRequest(format!("bad HH:MM value '{hhmm}'")));
    }
    let mut date = after.to_zoned(tz.clone()).date();
    for _ in 0..8 {
        let dt = date.at(h, m, 0, 0);
        let z = civil_to_zoned(tz, dt);
        let is_weekday = matches!(
            date.weekday(),
            jiff::civil::Weekday::Monday
                | jiff::civil::Weekday::Tuesday
                | jiff::civil::Weekday::Wednesday
                | jiff::civil::Weekday::Thursday
                | jiff::civil::Weekday::Friday
        );
        let ok_day = match recurrence {
            "weekdays" => is_weekday,
            _ => true,
        };
        if ok_day && z.timestamp() > after {
            return Ok(z);
        }
        date = date
            .tomorrow()
            .map_err(|e| ApiError::internal_msg(e.to_string()))?;
    }
    Err(ApiError::BadRequest(
        "no occurrence found within 8 days".into(),
    ))
}

/// Resolve an alarm spec into a concrete UTC instant + display string.
pub fn resolve(
    tz: &TimeZone,
    at_local: Option<&str>,
    in_minutes: Option<i64>,
    recurrence: &str,
    now: jiff::Timestamp,
) -> Result<Resolved, ApiError> {
    let mut notes = Vec::new();
    if let Some(mins) = in_minutes {
        if !(1..=7 * 24 * 60).contains(&mins) {
            return Err(ApiError::BadRequest("in_minutes must be 1..10080".into()));
        }
        let due = now.as_second() + mins * 60;
        let local = jiff::Timestamp::from_second(due)
            .map_err(|e| ApiError::internal_msg(e.to_string()))?
            .to_zoned(tz.clone());
        return Ok(Resolved {
            due_utc: due,
            local_display: local.strftime("%Y-%m-%d %H:%M %Z").to_string(),
            local_time: None,
            notes,
        });
    }
    let Some(at) = at_local else {
        return Err(ApiError::BadRequest(
            "alarm needs at_local ('YYYY-MM-DD HH:MM' or 'HH:MM') or in_minutes".into(),
        ));
    };
    let trimmed = at.trim();
    let (zoned, local_time) = if trimmed.len() <= 5 && trimmed.contains(':') {
        // "HH:MM" — today or the next matching day for recurrences.
        let z = next_clock_time(tz, trimmed, recurrence, now)?;
        (z, Some(trimmed.to_string()))
    } else {
        let dt = parse_local(trimmed)?;
        let z = civil_to_zoned(tz, dt);
        if recurrence == "none" {
            (z, None)
        } else {
            // recurring: keep the clock-time component.
            let hhmm = dt.strftime("%H:%M").to_string();
            let z2 = next_clock_time(tz, &hhmm, recurrence, now)?;
            (z2, Some(hhmm))
        }
    };
    if zoned.timestamp() <= now {
        return Err(ApiError::BadRequest(format!(
            "requested time {} is in the past",
            zoned.strftime("%Y-%m-%d %H:%M %Z")
        )));
    }
    // Detect ambiguous/nonexistent input for an honest note.
    if let Ok(dt) = parse_local(trimmed)
        && tz.to_ambiguous_zoned(dt).unambiguous().is_err()
    {
        notes.push(
            "local time was ambiguous or nonexistent (DST); resolved forward/earlier".to_string(),
        );
    }
    Ok(Resolved {
        due_utc: zoned.timestamp().as_second(),
        local_display: zoned.strftime("%Y-%m-%d %H:%M %Z").to_string(),
        local_time,
        notes,
    })
}

// ---------------------------------------------------------------------------
// CRUD used by API + tools
// ---------------------------------------------------------------------------

pub struct AlarmSpec<'a> {
    pub label: &'a str,
    pub kind: &'a str,
    pub original_request: String,
    pub at_local: Option<&'a str>,
    pub in_minutes: Option<i64>,
    pub recurrence: &'a str,
    pub endpoint_id: Option<&'a str>,
    pub volume: f64,
    pub snooze_minutes: i64,
}

/// Commit the alarm durably, then return it — confirmation happens only
/// after this function succeeds.
pub async fn create(
    state: &AppState,
    owner_id: i64,
    spec: AlarmSpec<'_>,
) -> Result<Value, ApiError> {
    let tz_name = state
        .get_setting("timezone")
        .await?
        .unwrap_or_else(|| "UTC".into());
    let tz = TimeZone::get(&tz_name)
        .map_err(|_| ApiError::BadRequest(format!("unknown timezone {tz_name}")))?;
    if !matches!(spec.recurrence, "none" | "daily" | "weekdays") {
        return Err(ApiError::BadRequest(
            "recurrence must be none|daily|weekdays".into(),
        ));
    }
    if !matches!(spec.kind, "alarm" | "timer") {
        return Err(ApiError::BadRequest("kind must be alarm|timer".into()));
    }
    let now_ts = jiff::Timestamp::now();
    let resolved = resolve(&tz, spec.at_local, spec.in_minutes, spec.recurrence, now_ts)?;

    // A playable endpoint is mandatory — an alarm that cannot make sound is
    // a limitation to report, not a success to pretend.
    let endpoint = pick_endpoint(&state.db, spec.endpoint_id).await?;
    let Some(ep) = endpoint else {
        return Err(ApiError::NotConfigured(
            "no enabled audio endpoint — add a speaker or browser audio endpoint first".into(),
        ));
    };

    let id = util::new_id();
    let now = util::now_unix();
    state
        .db
        .exec(
            "INSERT INTO alarms(id,owner_id,label,kind,original_request,timezone,local_time,
                recurrence,next_due_utc,endpoint_id,volume,status,snooze_minutes,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'scheduled',?12,?13,?13)",
            vec![
                P::T(id.clone()),
                P::I(owner_id),
                P::T(spec.label.to_string()),
                P::T(spec.kind.to_string()),
                P::T(spec.original_request.clone()),
                P::T(tz_name),
                resolved.local_time.clone().map(P::T).unwrap_or(P::Null),
                P::T(spec.recurrence.to_string()),
                P::I(resolved.due_utc),
                P::T(ep.0.clone()),
                P::F(spec.volume.clamp(0.0, 1.0)),
                P::I(spec.snooze_minutes.clamp(1, 60)),
                P::I(now),
            ],
        )
        .await?;
    state.scheduler.wake();
    state.emit("alarm", json!({"event": "created", "alarm_id": id}));
    Ok(json!({
        "id": id,
        "due_utc": resolved.due_utc,
        "resolved_local": resolved.local_display,
        "endpoint": ep.1,
        "notes": resolved.notes,
        "persisted": true,
    }))
}

/// endpoint_id -> (id, name, kind, config)
async fn pick_endpoint(
    db: &Db,
    wanted: Option<&str>,
) -> Result<Option<(String, String, String, Value)>, ApiError> {
    let row = match wanted {
        Some(id) => {
            db.q1(
                "SELECT id,name,kind,config FROM endpoints WHERE id = ?1 AND enabled = 1",
                vec![P::T(id.to_string())],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .await?
        }
        None => {
            db.q1(
                "SELECT id,name,kind,config FROM endpoints WHERE enabled = 1
                 ORDER BY created_at LIMIT 1",
                vec![],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .await?
        }
    };
    match row {
        Some((id, name, kind, cfg)) => Ok(Some((
            id,
            name,
            kind.clone(),
            serde_json::from_str(&cfg).unwrap_or(json!({})),
        ))),
        None => match wanted {
            Some(id) => Err(ApiError::BadRequest(format!(
                "audio endpoint '{id}' does not exist or is disabled"
            ))),
            None => Ok(None),
        },
    }
}

pub async fn list_for(db: &Db, requester_id: i64, is_owner: bool) -> Result<Vec<Value>, ApiError> {
    let vis = if is_owner {
        ("1=1".to_string(), vec![])
    } else {
        ("a.owner_id = ?".to_string(), vec![P::I(requester_id)])
    };
    let (clause, args) = vis;
    db.q(
        &format!(
            "SELECT a.id, a.label, a.kind, a.timezone, a.recurrence, a.next_due_utc,
                    a.status, a.endpoint_id, e.name, a.volume,
                    (SELECT status FROM alarm_deliveries d
                      WHERE d.alarm_id = a.id ORDER BY started_at DESC LIMIT 1),
                    (SELECT detail FROM alarm_deliveries d
                      WHERE d.alarm_id = a.id ORDER BY started_at DESC LIMIT 1),
                    u.username, a.snooze_minutes
             FROM alarms a
             LEFT JOIN endpoints e ON e.id = a.endpoint_id
             JOIN users u ON u.id = a.owner_id
             WHERE a.deleted_at IS NULL AND {clause}
             ORDER BY a.next_due_utc IS NULL, a.next_due_utc"
        ),
        args,
        |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "label": r.get::<_, String>(1)?,
                "kind": r.get::<_, String>(2)?,
                "timezone": r.get::<_, String>(3)?,
                "recurrence": r.get::<_, String>(4)?,
                "next_due_utc": r.get::<_, Option<i64>>(5)?,
                "status": r.get::<_, String>(6)?,
                "endpoint_id": r.get::<_, Option<String>>(7)?,
                "endpoint_name": r.get::<_, Option<String>>(8)?,
                "volume": r.get::<_, f64>(9)?,
                "last_delivery_status": r.get::<_, Option<String>>(10)?,
                "last_delivery_detail": r.get::<_, Option<String>>(11)?,
                "owner": r.get::<_, String>(12)?,
                "snooze_minutes": r.get::<_, i64>(13)?,
            }))
        },
    )
    .await
}

async fn load_alarm(db: &Db, id: &str) -> Result<AlarmRow, ApiError> {
    db.q1(
        &format!("SELECT {ALARM_COLS} FROM alarms WHERE id = ?1 AND deleted_at IS NULL"),
        vec![P::T(id.to_string())],
        map_alarm,
    )
    .await?
    .ok_or_else(|| ApiError::NotFound("alarm".into()))
}

fn check_ownership(alarm: &AlarmRow, requester_id: i64, is_owner: bool) -> Result<(), ApiError> {
    if is_owner || alarm.owner_id == requester_id {
        Ok(())
    } else {
        Err(ApiError::Forbidden("not your alarm".into()))
    }
}

pub async fn snooze(
    state: &AppState,
    id: &str,
    requester_id: i64,
    is_owner: bool,
) -> Result<Value, ApiError> {
    let alarm = load_alarm(&state.db, id).await?;
    check_ownership(&alarm, requester_id, is_owner)?;
    if alarm.status != "ringing" && alarm.status != "scheduled" {
        return Err(ApiError::BadRequest(format!(
            "alarm is '{}' — only ringing/scheduled alarms snooze",
            alarm.status
        )));
    }
    let new_due = util::now_unix() + alarm.snooze_minutes * 60;
    let delivering = state
        .db
        .call({
            let id = id.to_string();
            move |c| {
                let tx = c.transaction()?;
                let delivering = delivering_ids(&tx, &id)?;
                tx.execute(
                    "UPDATE alarm_deliveries SET status = 'canceled',
                        detail = detail || ' (snoozed)', finished_at = ?1
                     WHERE alarm_id = ?2 AND status = 'delivering'",
                    rusqlite::params![util::now_unix(), id],
                )?;
                tx.execute(
                    "UPDATE alarms SET status='scheduled', next_due_utc=?1,
                        updated_at=?2, schedule_version=schedule_version+1
                     WHERE id=?3",
                    rusqlite::params![new_due, util::now_unix(), id],
                )?;
                tx.commit()?;
                Ok(delivering)
            }
        })
        .await?;
    release_deliveries(state, &delivering);
    state.scheduler.wake();
    state.emit("alarm", json!({"event": "snoozed", "alarm_id": id}));
    Ok(json!({"id": id, "status": "scheduled", "next_due_utc": new_due}))
}

/// Stop ('done') ends this ringing: a recurring alarm goes on to its next
/// occurrence, and a Stop that arrives after the ringing ended changes
/// nothing. Cancel ('canceled') is terminal.
pub async fn stop_or_cancel(
    state: &AppState,
    id: &str,
    requester_id: i64,
    is_owner: bool,
    final_status: &str,
) -> Result<(), ApiError> {
    let alarm = load_alarm(&state.db, id).await?;
    check_ownership(&alarm, requester_id, is_owner)?;
    let now = util::now_unix();
    let stopping = final_status == "done";
    let next = if stopping {
        next_occurrence_after(&alarm, now.max(alarm.next_due_utc.unwrap_or(now)))
    } else {
        None
    };
    let new_status = if next.is_some() {
        "scheduled".to_string()
    } else {
        final_status.to_string()
    };
    let id2 = id.to_string();
    let (delivering, changed) = state
        .db
        .call(move |c| {
            let tx = c.transaction()?;
            let delivering = delivering_ids(&tx, &id2)?;
            tx.execute(
                "UPDATE alarm_deliveries SET status='canceled',
                    detail = detail || ' (stopped by user)', finished_at=?1
                 WHERE alarm_id=?2 AND status='delivering'",
                rusqlite::params![now, id2],
            )?;
            let only_if_ringing = if stopping {
                " AND status='ringing'"
            } else {
                ""
            };
            let changed = tx.execute(
                &format!(
                    "UPDATE alarms SET status=?1, next_due_utc=?2, updated_at=?3,
                        schedule_version=schedule_version+1
                     WHERE id=?4{only_if_ringing}"
                ),
                rusqlite::params![new_status, next, now, id2],
            )?;
            tx.commit()?;
            Ok((delivering, changed))
        })
        .await?;
    release_deliveries(state, &delivering);
    if changed > 0 {
        if next.is_some() {
            state.scheduler.wake();
        }
        state.emit("alarm", json!({"event": final_status, "alarm_id": id}));
    }
    Ok(())
}

/// Deliveries still in flight for an alarm. Read in the transaction that
/// ends them, so `release_deliveries` names exactly those.
fn delivering_ids(c: &rusqlite::Connection, alarm_id: &str) -> rusqlite::Result<Vec<String>> {
    let mut st =
        c.prepare("SELECT id FROM alarm_deliveries WHERE alarm_id=?1 AND status='delivering'")?;
    let ids = st
        .query_map([alarm_id], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(ids)
}

/// Silence deliveries the user just ended and release the executor, which
/// would otherwise hold every other alarm until the ack timeout. `finish`
/// then finds them settled and leaves the user's outcome in place.
fn release_deliveries(state: &AppState, delivery_ids: &[String]) {
    for id in delivery_ids {
        state.audio.broadcast_stop(id);
        state
            .audio
            .resolve_ack_status(id, true, "stopped by user".into());
    }
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

pub fn spawn(state: Arc<AppState>) -> SchedulerHandle {
    let handle = SchedulerHandle::detached();
    handle.clone().start(state);
    handle
}

async fn executor(state: Arc<AppState>, notify: Arc<Notify>) {
    reconcile_interrupted(state.as_ref(), "server restart").await;
    loop {
        let next = next_due(&state.db).await;
        match next {
            Some(t) if t <= util::now_unix() => {
                if let Err(e) = claim_and_deliver(state.as_ref()).await {
                    tracing::error!(error = %e, "alarm delivery failed");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
            Some(t) => {
                let wait = wait_for(t, util::now_unix());
                tokio::select! {
                    _ = notify.notified() => {}
                    _ = tokio::time::sleep(wait) => {}
                }
            }
            None => {
                tokio::select! {
                    _ = notify.notified() => {}
                    _ = tokio::time::sleep(MAX_SLEEP) => {}
                }
            }
        }
    }
}

/// Longest the executor sleeps before looking at the clock again. A timer
/// does not count time the host was suspended and does not follow clock
/// steps, so an alarm can only be as late as this after either.
const MAX_SLEEP: Duration = Duration::from_secs(60);

fn wait_for(due: i64, now: i64) -> Duration {
    Duration::from_secs((due - now).max(0) as u64).min(MAX_SLEEP)
}

async fn next_due(db: &Db) -> Option<i64> {
    db.q1(
        "SELECT MIN(next_due_utc) FROM alarms
         WHERE status = 'scheduled' AND deleted_at IS NULL",
        vec![],
        |r| r.get::<_, Option<i64>>(0),
    )
    .await
    .ok()
    .flatten()
    .flatten()
}

/// Crash/restart reconciliation: an alarm left 'ringing' was claimed but its
/// delivery outcome is unknown — the delivery record exists, so do NOT
/// redeliver; record the ambiguous outcome honestly. Recurring alarms are
/// rescheduled to their next occurrence rather than left dead.
async fn reconcile_interrupted(state: &AppState, cause: &str) {
    let now = util::now_unix();
    let _ = state
        .db
        .exec(
            "UPDATE alarm_deliveries SET status='failed',
                detail = detail || ' (interrupted by ' || ?2 || ')', finished_at=?1
             WHERE status='delivering'",
            vec![P::I(now), P::T(cause.to_string())],
        )
        .await;
    let ringing = state
        .db
        .q(
            &format!(
                "SELECT {ALARM_COLS} FROM alarms WHERE status='ringing' AND deleted_at IS NULL"
            ),
            vec![],
            map_alarm,
        )
        .await
        .unwrap_or_default();
    for alarm in ringing {
        let next = next_occurrence_after(&alarm, now.max(alarm.next_due_utc.unwrap_or(now)));
        let status = if next.is_some() { "scheduled" } else { "done" };
        let _ = state
            .db
            .exec(
                "UPDATE alarms SET status=?1, next_due_utc=?2, updated_at=?3 WHERE id=?4",
                vec![
                    P::T(status.to_string()),
                    next.map(P::I).unwrap_or(P::Null),
                    P::I(now),
                    P::T(alarm.id),
                ],
            )
            .await;
    }
}

/// A restore rewinds the alarms to when the backup was taken. A ring caught
/// in that backup has no delivery running now, the executor is asleep on the
/// old schedule, and satellites hold replicas of it.
pub async fn after_restore(state: &AppState) {
    reconcile_interrupted(state, "database restore").await;
    state.scheduler.wake();
    state.emit("alarm", json!({"event": "restored"}));
}

/// What became of a due alarm when the executor reached it.
enum Claim {
    /// It is ringing, under this delivery id.
    Ringing(String),
    /// That occurrence already has a delivery, so the alarm moved past it.
    Skipped,
    /// It is no longer due: each alarm ahead of it in the batch can hold the
    /// executor for the whole ack timeout, and someone snoozed or canceled it
    /// meanwhile.
    Changed,
}

/// Atomically claim due alarms (delivery row + status change in one
/// transaction — dedup key UNIQUE(alarm_id, occurrence_utc)), then deliver.
async fn claim_and_deliver(state: &AppState) -> Result<(), ApiError> {
    let now = util::now_unix();
    let due = state
        .db
        .q(
            &format!(
                "SELECT {ALARM_COLS} FROM alarms
                 WHERE status='scheduled' AND next_due_utc <= ?1 AND deleted_at IS NULL
                 ORDER BY next_due_utc LIMIT 10"
            ),
            vec![P::I(now)],
            map_alarm,
        )
        .await?;
    for alarm in due {
        let occurrence = alarm.next_due_utc.unwrap_or(now);
        let next = next_occurrence_after(&alarm, now.max(occurrence));
        let claimed = state
            .db
            .call({
                let alarm = alarm.clone();
                move |c| {
                    let tx = c.transaction()?;
                    let rung = tx.execute(
                        "UPDATE alarms SET status='ringing', updated_at=?1
                         WHERE id=?2 AND status='scheduled' AND next_due_utc=?3",
                        rusqlite::params![now, alarm.id, occurrence],
                    )?;
                    if rung == 0 {
                        return Ok(Claim::Changed);
                    }
                    let exec_id = util::new_id();
                    let delivery_id = util::new_id();
                    let inserted = tx.execute(
                        "INSERT INTO alarm_deliveries(id,alarm_id,occurrence_utc,execution_id,
                            endpoint_id,status,started_at)
                         VALUES(?1,?2,?3,?4,?5,'delivering',?6)
                         ON CONFLICT(alarm_id,occurrence_utc) DO NOTHING",
                        rusqlite::params![
                            delivery_id,
                            alarm.id,
                            occurrence,
                            exec_id,
                            alarm.endpoint_id,
                            now
                        ],
                    )?;
                    if inserted == 0 {
                        // Already delivered for this occurrence (a satellite
                        // ahead of this clock rang it). Move past it, or the
                        // executor would keep finding it due.
                        let status = if next.is_some() { "scheduled" } else { "done" };
                        tx.execute(
                            "UPDATE alarms SET status=?1, next_due_utc=?2, updated_at=?3,
                                schedule_version=schedule_version+1
                             WHERE id=?4",
                            rusqlite::params![status, next, now, alarm.id],
                        )?;
                        tx.commit()?;
                        return Ok(Claim::Skipped);
                    }
                    tx.commit()?;
                    Ok(Claim::Ringing(delivery_id))
                }
            })
            .await?;
        match claimed {
            Claim::Ringing(delivery_id) => {
                state.emit(
                    "alarm",
                    json!({"event": "ringing", "alarm_id": alarm.id, "label": alarm.label}),
                );
                deliver(state, alarm, delivery_id, occurrence).await;
            }
            // Satellites holding this occurrence need the new schedule.
            Claim::Skipped => state.emit(
                "alarm",
                json!({"event": "skipped", "alarm_id": alarm.id, "occurrence_utc": occurrence}),
            ),
            Claim::Changed => {}
        }
    }
    Ok(())
}

/// Next occurrence of a recurring alarm strictly after `now`; `None` for
/// one-shot alarms (which are then done). To move past an occurrence, pass
/// `now.max(occurrence)`: a clock still behind it would land back on it, and
/// its delivery row would then block the alarm for good.
fn next_occurrence_after(alarm: &AlarmRow, now: i64) -> Option<i64> {
    if alarm.recurrence == "none" {
        return None;
    }
    let (Some(hhmm), Ok(tz)) = (alarm.local_time.clone(), TimeZone::get(&alarm.timezone)) else {
        return None;
    };
    next_clock_time(
        &tz,
        &hhmm,
        &alarm.recurrence,
        jiff::Timestamp::from_second(now).unwrap_or_else(|_| jiff::Timestamp::now()),
    )
    .ok()
    .map(|z| z.timestamp().as_second())
}

async fn finish(
    state: &AppState,
    alarm: &AlarmRow,
    delivery_id: &str,
    occurrence: i64,
    status: &str,
    detail: &str,
) {
    let now = util::now_unix();
    let next = next_occurrence_after(alarm, now.max(occurrence));
    let new_status = if next.is_some() { "scheduled" } else { "done" };
    let aid = alarm.id.clone();
    let did = delivery_id.to_string();
    let st = status.to_string();
    let det = detail.to_string();
    // The two guards are independent. A snooze, stop or cancel ends both the
    // delivery and the ringing, and those outcomes stand. A satellite's
    // local report settles only the delivery, and the alarm must still move
    // on from 'ringing'.
    let settled = state
        .db
        .call(move |c| {
            let tx = c.transaction()?;
            let delivery = tx.execute(
                "UPDATE alarm_deliveries SET status=?1, detail=?2, finished_at=?3
                 WHERE id=?4 AND status='delivering'",
                rusqlite::params![st, det, now, did],
            )?;
            let alarm = tx.execute(
                "UPDATE alarms SET status=?1, next_due_utc=?2, updated_at=?3,
                    schedule_version=schedule_version+1
                 WHERE id=?4 AND status='ringing'",
                rusqlite::params![new_status, next, now, aid],
            )?;
            tx.commit()?;
            Ok((delivery > 0, alarm > 0))
        })
        .await;
    match settled {
        Ok((true, _)) => state.emit(
            "alarm",
            json!({
                "event": "delivered",
                "alarm_id": alarm.id,
                "status": status,
                "detail": detail,
                "occurrence_utc": occurrence,
            }),
        ),
        // The delivery was settled and reported elsewhere; only the schedule
        // moved, and this outcome was never stored.
        Ok((false, true)) => state.emit(
            "alarm",
            json!({"event": "rescheduled", "alarm_id": alarm.id}),
        ),
        Ok((false, false)) => {}
        Err(e) => tracing::error!(alarm = %alarm.id, error = %e, "recording alarm outcome failed"),
    }
}

async fn deliver(state: &AppState, alarm: AlarmRow, delivery_id: String, occurrence: i64) {
    let late_note = if util::now_unix() - occurrence > 60 {
        " (fired late — server was down or busy)"
    } else {
        ""
    };
    let ep = match pick_endpoint(&state.db, alarm.endpoint_id.as_deref()).await {
        Ok(Some(e)) => e,
        Ok(None) => {
            finish(
                state,
                &alarm,
                &delivery_id,
                occurrence,
                "failed",
                "no enabled audio endpoint at due time",
            )
            .await;
            return;
        }
        Err(e) => {
            finish(
                state,
                &alarm,
                &delivery_id,
                occurrence,
                "failed",
                &format!("endpoint error: {e}"),
            )
            .await;
            return;
        }
    };
    let (_ep_id, ep_name, ep_kind, cfg) = ep;
    let token = state.audio.grant_media("builtin:tone");
    let (status, detail): (String, String) = match ep_kind.as_str() {
        "ha_media_player" => {
            let entity = cfg
                .get("entity_id")
                .and_then(|e| e.as_str())
                .unwrap_or("")
                .to_string();
            if entity.is_empty() {
                (
                    "failed".into(),
                    format!("endpoint '{ep_name}' has no entity_id"),
                )
            } else {
                let base = state
                    .public_base()
                    .await
                    .unwrap_or_else(|_| state.config.base_url());
                let url = format!("{}/media/{token}", base.trim_end_matches('/'));
                match state.ha.play_media(&entity, &url, Some(alarm.volume)).await {
                    Ok(()) => (
                        "played".into(),
                        format!(
                            "playback requested on {entity} via Home Assistant (accepted; audibility not confirmed){late_note}"
                        ),
                    ),
                    Err(e) => (
                        "failed".into(),
                        format!("home assistant playback failed: {e}"),
                    ),
                }
            }
        }
        "browser" => {
            let channel = cfg
                .get("channel_id")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let cmd = AudioCommand::Play {
                delivery_id: delivery_id.clone(),
                url: format!("/media/{token}"),
                volume: alarm.volume,
                label: if alarm.label.is_empty() {
                    "Alarm".into()
                } else {
                    alarm.label.clone()
                },
            };
            // Arm the ack waiter before dispatching so a fast client ack
            // cannot land between send and wait.
            let ack = state.audio.arm_ack(&delivery_id);
            match state.audio.send_to_channel(&channel, &cmd) {
                Ok(()) => match state.audio.await_ack(ack, Duration::from_secs(20)).await {
                    Ok((true, result)) => (
                        "played".into(),
                        format!("played on browser endpoint '{ep_name}' ({result}){late_note}"),
                    ),
                    Ok((false, result)) => (
                        "failed".into(),
                        format!("browser playback failed: {result}"),
                    ),
                    Err(e) => (
                        "failed".into(),
                        format!("browser endpoint did not ack: {e}"),
                    ),
                },
                Err(e) => ("failed".into(), format!("browser channel unavailable: {e}")),
            }
        }
        "satellite" => {
            let sat_id = cfg
                .get("satellite_id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if sat_id.is_empty() {
                (
                    "failed".into(),
                    format!("endpoint '{ep_name}' has no satellite_id"),
                )
            } else {
                // Push playback over the live session; the satellite fetches
                // the tokenized media URL and acks with request_id=delivery.
                // The alarm reference lets a satellite that already rang this
                // occurrence from its local replica ack without ringing twice.
                let mut env = adrian_protocol::Envelope::new(adrian_protocol::Message::Speak {
                    media_url: format!("/media/{token}"),
                    text: Some(if alarm.label.is_empty() {
                        "Alarm".into()
                    } else {
                        alarm.label.clone()
                    }),
                    alarm: Some(adrian_protocol::AlarmRef {
                        alarm_id: alarm.id.clone(),
                        occurrence_unix: occurrence,
                    }),
                });
                env.request_id = delivery_id.clone();
                env.idempotency_key = Some(delivery_id.clone());
                let ack = state.audio.arm_ack(&delivery_id);
                match state.satellites.send(&sat_id, &env) {
                    Ok(()) => match state.audio.await_ack(ack, Duration::from_secs(20)).await {
                        Ok((true, result)) => (
                            "played".into(),
                            format!(
                                "played on satellite endpoint '{ep_name}' ({result}){late_note}"
                            ),
                        ),
                        Ok((false, result)) => (
                            "failed".into(),
                            format!("satellite playback failed: {result}"),
                        ),
                        Err(e) => (
                            "failed".into(),
                            format!("satellite endpoint did not ack: {e}"),
                        ),
                    },
                    Err(e) => (
                        "failed".into(),
                        format!("satellite session unavailable: {e}"),
                    ),
                }
            }
        }
        other => (
            "failed".into(),
            format!("endpoint kind '{other}' not supported for delivery"),
        ),
    };
    finish(state, &alarm, &delivery_id, occurrence, &status, &detail).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tz(name: &str) -> TimeZone {
        TimeZone::get(name).unwrap()
    }

    #[test]
    fn timer_resolves_to_now_plus_minutes() {
        let now = jiff::Timestamp::from_second(1_800_000_000).unwrap();
        let r = resolve(&tz("UTC"), None, Some(45), "none", now).unwrap();
        assert_eq!(r.due_utc, 1_800_000_000 + 45 * 60);
    }

    #[test]
    fn timer_bounds_are_enforced() {
        let now = jiff::Timestamp::now();
        assert!(resolve(&tz("UTC"), None, Some(0), "none", now).is_err());
        assert!(resolve(&tz("UTC"), None, Some(10081), "none", now).is_err());
    }

    #[test]
    fn hhmm_rolls_to_tomorrow_when_past() {
        // 10:00 UTC, ask for 09:00 -> tomorrow 09:00.
        let now: jiff::Timestamp = "2026-09-17T10:00:00Z".parse().unwrap();
        let r = resolve(&tz("UTC"), Some("09:00"), None, "none", now).unwrap();
        let due = jiff::Timestamp::from_second(r.due_utc)
            .unwrap()
            .to_zoned(TimeZone::UTC);
        assert_eq!(
            due.strftime("%Y-%m-%d %H:%M").to_string(),
            "2026-09-18 09:00"
        );
    }

    #[test]
    fn explicit_datetime_is_used_verbatim() {
        let now: jiff::Timestamp = "2026-09-17T10:00:00Z".parse().unwrap();
        let r = resolve(&tz("UTC"), Some("2026-09-20 07:30"), None, "none", now).unwrap();
        let due = jiff::Timestamp::from_second(r.due_utc)
            .unwrap()
            .to_zoned(TimeZone::UTC);
        assert_eq!(
            due.strftime("%Y-%m-%d %H:%M").to_string(),
            "2026-09-20 07:30"
        );
    }

    #[test]
    fn weekdays_skip_weekend() {
        // Friday Sep 18 2026, 17:00 UTC; ask for 09:00 weekdays -> Monday Sep 21.
        let now: jiff::Timestamp = "2026-09-18T17:00:00Z".parse().unwrap();
        let r = resolve(&tz("UTC"), Some("09:00"), None, "weekdays", now).unwrap();
        let due = jiff::Timestamp::from_second(r.due_utc)
            .unwrap()
            .to_zoned(TimeZone::UTC);
        assert_eq!(due.strftime("%Y-%m-%d").to_string(), "2026-09-21");
        assert_eq!(due.weekday(), jiff::civil::Weekday::Monday);
    }

    #[test]
    fn dst_gap_springs_forward() {
        // America/New_York 2026-03-08: 02:30 does not exist (spring forward).
        // 'compatible' resolves forward to 03:30 EDT.
        let now: jiff::Timestamp = "2026-03-07T12:00:00Z".parse().unwrap();
        let r = resolve(
            &tz("America/New_York"),
            Some("2026-03-08 02:30"),
            None,
            "none",
            now,
        )
        .unwrap();
        let due = jiff::Timestamp::from_second(r.due_utc)
            .unwrap()
            .to_zoned(tz("America/New_York"));
        assert_eq!(due.strftime("%H:%M").to_string(), "03:30");
    }

    #[test]
    fn bad_input_is_rejected() {
        let now = jiff::Timestamp::now();
        assert!(resolve(&tz("UTC"), Some("25:99"), None, "none", now).is_err());
        assert!(resolve(&tz("UTC"), None, None, "none", now).is_err());
        assert!(resolve(&tz("UTC"), Some("not a time"), None, "none", now).is_err());
    }

    /// Mid-delivery crash: 'ringing' alarms + 'delivering' rows must be
    /// reconciled honestly (no silent refire) and recurring alarms must be
    /// rescheduled rather than left dead.
    #[tokio::test]
    async fn reconcile_recovers_interrupted_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::Config {
            data_dir: dir.path().to_path_buf(),
            bind: "127.0.0.1:0".parse().unwrap(),
            web_dir: dir.path().to_path_buf(),
            tls: None,
            trusted_proxy: None,
        };
        let state = crate::build_state(config).await.unwrap();
        let now = util::now_unix();
        let uid: i64 = state
            .db
            .call(|c| {
                c.execute(
                    "INSERT INTO users(username,display_name,password_hash,role,created_at)
                     VALUES('o','o','x','owner',1)",
                    [],
                )?;
                Ok(c.last_insert_rowid())
            })
            .await
            .unwrap();
        // One-off alarm claimed at crash: becomes 'done', delivery 'failed'.
        state
            .db
            .exec(
                "INSERT INTO alarms(id,owner_id,label,kind,timezone,recurrence,next_due_utc,status,created_at,updated_at)
                 VALUES('one',?1,'t','alarm','UTC','none',?2,'ringing',1,1)",
                vec![P::I(uid), P::I(now - 60)],
            )
            .await
            .unwrap();
        state
            .db
            .exec(
                "INSERT INTO alarm_deliveries(id,alarm_id,occurrence_utc,execution_id,status,started_at)
                 VALUES('d1','one',?1,'e','delivering',?1)",
                vec![P::I(now - 60)],
            )
            .await
            .unwrap();
        // Recurring alarm claimed at crash: must be rescheduled, not dead.
        state
            .db
            .exec(
                "INSERT INTO alarms(id,owner_id,label,kind,timezone,local_time,recurrence,next_due_utc,status,created_at,updated_at)
                 VALUES('rec',?1,'t','alarm','UTC','09:00','daily',?2,'ringing',1,1)",
                vec![P::I(uid), P::I(now - 60)],
            )
            .await
            .unwrap();

        reconcile_interrupted(state.as_ref(), "server restart").await;

        let one = state
            .db
            .q1(
                "SELECT status, next_due_utc FROM alarms WHERE id='one'",
                vec![],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(one.0, "done");
        assert_eq!(one.1, None);
        let delivery: String = state
            .db
            .q1(
                "SELECT status FROM alarm_deliveries WHERE id='d1'",
                vec![],
                |r| r.get(0),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivery, "failed");
        let rec = state
            .db
            .q1(
                "SELECT status, next_due_utc FROM alarms WHERE id='rec'",
                vec![],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rec.0, "scheduled");
        assert!(rec.1.unwrap() > now);
        // The occurrence that was interrupted is NOT refired.
        let deliveries: i64 = state
            .db
            .q1(
                "SELECT COUNT(*) FROM alarm_deliveries WHERE alarm_id='rec'",
                vec![],
                |r| r.get(0),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(deliveries, 0); // no new delivery row created by reconciliation
    }

    /// State with no executor running, so a test sets the order of events
    /// itself, plus an owner to hang alarms on.
    async fn test_state() -> (tempfile::TempDir, Arc<AppState>, i64) {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::Config {
            data_dir: dir.path().to_path_buf(),
            bind: "127.0.0.1:0".parse().unwrap(),
            web_dir: dir.path().to_path_buf(),
            tls: None,
            trusted_proxy: None,
        };
        let state = crate::build_state(config).await.unwrap();
        let uid = state
            .db
            .call(|c| {
                c.execute(
                    "INSERT INTO users(username,display_name,password_hash,role,created_at)
                     VALUES('o','o','x','owner',1)",
                    [],
                )?;
                Ok(c.last_insert_rowid())
            })
            .await
            .unwrap();
        (dir, state, uid)
    }

    async fn seed_alarm(
        state: &AppState,
        uid: i64,
        id: &str,
        recurrence: &str,
        due: i64,
        status: &str,
    ) -> AlarmRow {
        let local_time = if recurrence == "none" {
            P::Null
        } else {
            P::T("07:00".into())
        };
        state
            .db
            .exec(
                "INSERT INTO alarms(id,owner_id,label,kind,timezone,local_time,recurrence,next_due_utc,status,created_at,updated_at)
                 VALUES(?1,?2,'t','alarm','UTC',?3,?4,?5,?6,1,1)",
                vec![
                    P::T(id.into()),
                    P::I(uid),
                    local_time,
                    P::T(recurrence.into()),
                    P::I(due),
                    P::T(status.into()),
                ],
            )
            .await
            .unwrap();
        load_alarm(&state.db, id).await.unwrap()
    }

    /// Delivery row for `alarm_id`, with the id `d-<alarm_id>`.
    async fn seed_delivery(state: &AppState, alarm_id: &str, occurrence: i64, status: &str) {
        state
            .db
            .exec(
                "INSERT INTO alarm_deliveries(id,alarm_id,occurrence_utc,execution_id,status,started_at)
                 VALUES('d-' || ?1,?1,?2,'e',?3,?2)",
                vec![
                    P::T(alarm_id.into()),
                    P::I(occurrence),
                    P::T(status.into()),
                ],
            )
            .await
            .unwrap();
    }

    /// An alarm mid-ring, as `claim_and_deliver` leaves it for `finish`.
    async fn seed_ringing(
        state: &AppState,
        uid: i64,
        id: &str,
        recurrence: &str,
        occurrence: i64,
    ) -> AlarmRow {
        let alarm = seed_alarm(state, uid, id, recurrence, occurrence, "ringing").await;
        seed_delivery(state, id, occurrence, "delivering").await;
        alarm
    }

    async fn alarm_state(state: &AppState, id: &str) -> (String, Option<i64>) {
        state
            .db
            .q1(
                "SELECT status, next_due_utc FROM alarms WHERE id = ?1",
                vec![P::T(id.into())],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .await
            .unwrap()
            .unwrap()
    }

    async fn delivery_state(state: &AppState, id: &str) -> (String, String) {
        state
            .db
            .q1(
                "SELECT status, detail FROM alarm_deliveries WHERE id = ?1",
                vec![P::T(id.into())],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .await
            .unwrap()
            .unwrap()
    }

    /// Whether `due` is the first 07:00 after `now`, on the schedule the
    /// seeded alarms use.
    fn is_next_seven_oclock(due: i64, now: i64) -> bool {
        due > now && due <= now + 86_400 && due % 86_400 == 7 * 3600
    }

    /// The next 07:00 after now, on the schedule the seeded alarms use.
    fn next_seven_oclock() -> i64 {
        let now = jiff::Timestamp::from_second(util::now_unix()).unwrap();
        next_clock_time(&tz("UTC"), "07:00", "daily", now)
            .unwrap()
            .timestamp()
            .as_second()
    }

    #[tokio::test]
    async fn snooze_while_ringing_survives_finish() {
        let (_dir, state, uid) = test_state().await;
        let occurrence = util::now_unix() - 1;
        let alarm = seed_ringing(&state, uid, "a", "none", occurrence).await;

        let snoozed = snooze(&state, "a", uid, true).await.unwrap();
        finish(&state, &alarm, "d-a", occurrence, "played", "played").await;

        assert_eq!(
            alarm_state(&state, "a").await,
            ("scheduled".to_string(), snoozed["next_due_utc"].as_i64())
        );
        assert_eq!(delivery_state(&state, "d-a").await.0, "canceled");
    }

    #[tokio::test]
    async fn cancel_while_ringing_survives_finish() {
        let (_dir, state, uid) = test_state().await;
        let occurrence = util::now_unix() - 1;
        let alarm = seed_ringing(&state, uid, "a", "daily", occurrence).await;

        stop_or_cancel(&state, "a", uid, true, "canceled")
            .await
            .unwrap();
        finish(&state, &alarm, "d-a", occurrence, "played", "played").await;

        assert_eq!(
            alarm_state(&state, "a").await,
            ("canceled".to_string(), None)
        );
        assert_eq!(delivery_state(&state, "d-a").await.0, "canceled");
    }

    /// Stop ends this ringing, not the schedule. `finish` no longer re-arms
    /// a stopped alarm, so Stop has to do that itself.
    #[tokio::test]
    async fn stop_keeps_a_recurring_schedule_and_ends_a_one_shot() {
        let (_dir, state, uid) = test_state().await;
        let now = util::now_unix();
        let daily = seed_ringing(&state, uid, "daily", "daily", now - 1).await;
        let once = seed_ringing(&state, uid, "once", "none", now - 1).await;

        stop_or_cancel(&state, "daily", uid, true, "done")
            .await
            .unwrap();
        stop_or_cancel(&state, "once", uid, true, "done")
            .await
            .unwrap();
        finish(&state, &daily, "d-daily", now - 1, "played", "played").await;
        finish(&state, &once, "d-once", now - 1, "played", "played").await;

        let (status, due) = alarm_state(&state, "daily").await;
        assert_eq!(status, "scheduled");
        assert!(is_next_seven_oclock(due.unwrap(), now));
        assert_eq!(delivery_state(&state, "d-daily").await.0, "canceled");
        assert_eq!(
            alarm_state(&state, "once").await,
            ("done".to_string(), None)
        );
    }

    /// A Stop that lands just after `finish` has nothing left to stop. It
    /// must not end tomorrow's alarm.
    #[tokio::test]
    async fn stop_after_the_ringing_ended_changes_nothing() {
        let (_dir, state, uid) = test_state().await;
        let due = next_seven_oclock();
        seed_alarm(&state, uid, "daily", "daily", due, "scheduled").await;

        stop_or_cancel(&state, "daily", uid, true, "done")
            .await
            .unwrap();

        assert_eq!(
            alarm_state(&state, "daily").await,
            ("scheduled".to_string(), Some(due))
        );
    }

    /// `record_local_firings` can settle the delivery while the executor
    /// still waits on the ack. The record stands, and the alarm must still
    /// leave 'ringing'.
    #[tokio::test]
    async fn finish_keeps_a_delivery_already_settled_and_still_advances_the_alarm() {
        let (_dir, state, uid) = test_state().await;
        let now = util::now_unix();
        let alarm = seed_ringing(&state, uid, "a", "daily", now - 1).await;
        state
            .db
            .exec(
                "UPDATE alarm_deliveries SET status='played', detail='rang locally' WHERE id='d-a'",
                vec![],
            )
            .await
            .unwrap();

        finish(&state, &alarm, "d-a", now - 1, "failed", "did not ack").await;

        assert_eq!(
            delivery_state(&state, "d-a").await,
            ("played".to_string(), "rang locally".to_string())
        );
        let (status, due) = alarm_state(&state, "a").await;
        assert_eq!(status, "scheduled");
        assert!(is_next_seven_oclock(due.unwrap(), now));
    }

    /// Stop, snooze and cancel end the delivery they name at once, so the
    /// executor is not held for the ack timeout, and leave every other
    /// browser alone.
    #[tokio::test]
    async fn ending_a_ringing_alarm_releases_its_delivery_and_no_other() {
        let (_dir, state, uid) = test_state().await;
        let now = util::now_unix();
        let mut rx = state.audio.attach_channel("c");
        for (id, end) in [
            ("stopped", "done"),
            ("canceled", "canceled"),
            ("snoozed", ""),
        ] {
            seed_ringing(&state, uid, id, "none", now - 1).await;
            let delivery = format!("d-{id}");
            let ack = state.audio.arm_ack(&delivery);

            if end.is_empty() {
                snooze(&state, id, uid, true).await.unwrap();
            } else {
                stop_or_cancel(&state, id, uid, true, end).await.unwrap();
            }

            let (ok, detail) = state
                .audio
                .await_ack(ack, Duration::from_secs(1))
                .await
                .unwrap();
            assert!(ok, "{detail}");
            assert_eq!(detail, "stopped by user");
            match rx.try_recv() {
                Ok(AudioCommand::Stop { delivery_id }) => assert_eq!(delivery_id, delivery),
                other => panic!("expected stop for {delivery}, got {other:?}"),
            }
        }

        // Nothing was ringing here, so nothing is sent.
        seed_alarm(&state, uid, "idle", "none", now + 3600, "scheduled").await;
        stop_or_cancel(&state, "idle", uid, true, "canceled")
            .await
            .unwrap();
        assert!(rx.try_recv().is_err());
    }

    /// A satellite running ahead of the server can report an occurrence the
    /// server has not reached: the delivery row exists while the alarm still
    /// points at it. Left there, the executor would spin on it forever.
    #[tokio::test]
    async fn delivery_conflict_moves_the_alarm_past_that_occurrence() {
        let (_dir, state, uid) = test_state().await;
        let now = util::now_unix();
        seed_alarm(&state, uid, "daily", "daily", now - 60, "scheduled").await;
        seed_delivery(&state, "daily", now - 60, "played").await;
        seed_alarm(&state, uid, "once", "none", now - 60, "scheduled").await;
        seed_delivery(&state, "once", now - 60, "played").await;

        claim_and_deliver(&state).await.unwrap();

        let (status, due) = alarm_state(&state, "daily").await;
        assert_eq!(status, "scheduled");
        assert!(due.unwrap() > now);
        assert_eq!(
            alarm_state(&state, "once").await,
            ("done".to_string(), None)
        );
        let deliveries = state
            .db
            .q1("SELECT COUNT(*) FROM alarm_deliveries", vec![], |r| {
                r.get::<_, i64>(0)
            })
            .await
            .unwrap();
        assert_eq!(deliveries, Some(2));
    }

    #[tokio::test]
    async fn local_firing_ahead_of_the_server_clock_moves_past_that_occurrence() {
        let (_dir, state, uid) = test_state().await;
        let due = next_seven_oclock();
        state
            .db
            .exec(
                r#"INSERT INTO endpoints(id,name,kind,config,created_at)
                   VALUES('ep','Sat','satellite','{"satellite_id":"s1"}',1)"#,
                vec![],
            )
            .await
            .unwrap();
        seed_alarm(&state, uid, "daily", "daily", due, "scheduled").await;
        state
            .db
            .exec("UPDATE alarms SET endpoint_id='ep'", vec![])
            .await
            .unwrap();

        let fired = [adrian_protocol::LocalAlarmFiring {
            alarm_id: "daily".into(),
            occurrence_unix: due,
            fired_at_unix: due,
        }];
        let recorded = replicas::record_local_firings(&state, "s1", "Sat", &fired)
            .await
            .unwrap();

        assert_eq!(recorded, 1);
        let (status, next) = alarm_state(&state, "daily").await;
        assert_eq!(status, "scheduled");
        assert!(next.unwrap() > due);
    }

    /// Same trap on boot: an interrupted occurrence still ahead of the clock
    /// must not be rescheduled onto itself.
    #[tokio::test]
    async fn reconcile_moves_past_an_occurrence_still_ahead_of_the_clock() {
        let (_dir, state, uid) = test_state().await;
        let due = next_seven_oclock();
        seed_ringing(&state, uid, "daily", "daily", due).await;

        reconcile_interrupted(&state, "server restart").await;

        let (status, next) = alarm_state(&state, "daily").await;
        assert_eq!(status, "scheduled");
        assert!(next.unwrap() > due);
    }

    /// A clock stepped back mid-ring: the occurrence is ahead of it again,
    /// and landing back on that occurrence would hit its own delivery row.
    #[tokio::test]
    async fn ending_a_ring_moves_past_an_occurrence_ahead_of_the_clock() {
        let (_dir, state, uid) = test_state().await;
        let due = next_seven_oclock();
        let finished = seed_ringing(&state, uid, "finished", "daily", due).await;
        seed_ringing(&state, uid, "stopped", "daily", due).await;

        finish(&state, &finished, "d-finished", due, "played", "played").await;
        stop_or_cancel(&state, "stopped", uid, true, "done")
            .await
            .unwrap();

        for id in ["finished", "stopped"] {
            let (status, next) = alarm_state(&state, id).await;
            assert_eq!(status, "scheduled", "{id}");
            assert!(next.unwrap() > due, "{id}");
        }
    }

    /// One delivery can hold the executor for the whole ack timeout, so the
    /// rest of its batch can change before their turn comes.
    #[tokio::test]
    async fn alarms_changed_while_their_batch_waits_are_not_rung() {
        let (_dir, state, uid) = test_state().await;
        let now = util::now_unix();
        state
            .db
            .exec(
                r#"INSERT INTO endpoints(id,name,kind,config,created_at)
                   VALUES('ep','Tab','browser','{"channel_id":"c"}',1)"#,
                vec![],
            )
            .await
            .unwrap();
        seed_alarm(&state, uid, "first", "none", now - 3, "scheduled").await;
        seed_alarm(&state, uid, "snoozed", "none", now - 2, "scheduled").await;
        seed_alarm(&state, uid, "canceled", "none", now - 1, "scheduled").await;
        state
            .db
            .exec("UPDATE alarms SET endpoint_id='ep'", vec![])
            .await
            .unwrap();
        let mut rx = state.audio.attach_channel("c");

        let batch = tokio::spawn({
            let state = state.clone();
            async move { claim_and_deliver(&state).await }
        });
        let delivery_id = match rx.recv().await {
            Ok(AudioCommand::Play { delivery_id, .. }) => delivery_id,
            other => panic!("expected play, got {other:?}"),
        };

        let snoozed = snooze(&state, "snoozed", uid, true).await.unwrap();
        stop_or_cancel(&state, "canceled", uid, true, "canceled")
            .await
            .unwrap();
        state
            .audio
            .resolve_ack_status(&delivery_id, true, "played".into());
        batch.await.unwrap().unwrap();

        assert_eq!(
            alarm_state(&state, "snoozed").await,
            ("scheduled".to_string(), snoozed["next_due_utc"].as_i64())
        );
        assert_eq!(
            alarm_state(&state, "canceled").await,
            ("canceled".to_string(), None)
        );
        let deliveries = state
            .db
            .q1("SELECT COUNT(*) FROM alarm_deliveries", vec![], |r| {
                r.get::<_, i64>(0)
            })
            .await
            .unwrap();
        assert_eq!(deliveries, Some(1), "only 'first' was rung");
    }

    #[test]
    fn executor_sleep_is_capped_so_it_keeps_checking_the_clock() {
        let now = 1_800_000_000;
        assert_eq!(wait_for(now + 8 * 3600, now), Duration::from_secs(60));
        assert_eq!(wait_for(now + 5, now), Duration::from_secs(5));
        assert_eq!(wait_for(now - 5, now), Duration::ZERO);
    }
}
