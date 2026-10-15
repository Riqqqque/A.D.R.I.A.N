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
                    u.username
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
    state
        .db
        .call({
            let id = id.to_string();
            move |c| {
                let tx = c.transaction()?;
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
                Ok(())
            }
        })
        .await?;
    state.scheduler.wake();
    state.emit("alarm", json!({"event": "snoozed", "alarm_id": id}));
    Ok(json!({"id": id, "status": "scheduled", "next_due_utc": new_due}))
}

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
    let id2 = id.to_string();
    let fs = final_status.to_string();
    state
        .db
        .call(move |c| {
            let tx = c.transaction()?;
            tx.execute(
                "UPDATE alarm_deliveries SET status='canceled',
                    detail = detail || ' (stopped by user)', finished_at=?1
                 WHERE alarm_id=?2 AND status='delivering'",
                rusqlite::params![now, id2],
            )?;
            tx.execute(
                "UPDATE alarms SET status=?1, next_due_utc=NULL, updated_at=?2,
                    schedule_version=schedule_version+1
                 WHERE id=?3",
                rusqlite::params![fs, now, id2],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    // Tell any browser endpoint to silence an in-progress playback.
    state.audio.broadcast_stop(id);
    state.emit("alarm", json!({"event": final_status, "alarm_id": id}));
    Ok(())
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
    reconcile_after_boot(state.as_ref()).await;
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
                let wait = Duration::from_secs((t - util::now_unix()).max(0) as u64);
                tokio::select! {
                    _ = notify.notified() => {}
                    _ = tokio::time::sleep(wait) => {}
                }
            }
            None => {
                tokio::select! {
                    _ = notify.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                }
            }
        }
    }
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
async fn reconcile_after_boot(state: &AppState) {
    let now = util::now_unix();
    let _ = state
        .db
        .exec(
            "UPDATE alarm_deliveries SET status='failed',
                detail = detail || ' (interrupted by server restart)', finished_at=?1
             WHERE status='delivering'",
            vec![P::I(now)],
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
        let next = if alarm.recurrence != "none" {
            match (alarm.local_time.clone(), TimeZone::get(&alarm.timezone)) {
                (Some(hhmm), Ok(tz)) => next_clock_time(
                    &tz,
                    &hhmm,
                    &alarm.recurrence,
                    jiff::Timestamp::from_second(now).unwrap_or_else(|_| jiff::Timestamp::now()),
                )
                .ok()
                .map(|z| z.timestamp().as_second()),
                _ => None,
            }
        } else {
            None
        };
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
        let claimed = state
            .db
            .call({
                let alarm = alarm.clone();
                move |c| {
                    let tx = c.transaction()?;
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
                        tx.commit()?;
                        return Ok(None); // already delivered for this occurrence
                    }
                    let updated = tx.execute(
                        "UPDATE alarms SET status='ringing', updated_at=?1
                         WHERE id=?2 AND status='scheduled'",
                        rusqlite::params![now, alarm.id],
                    )?;
                    tx.commit()?;
                    Ok(if updated == 1 {
                        Some((delivery_id, exec_id))
                    } else {
                        None
                    })
                }
            })
            .await?;
        let Some((delivery_id, _exec)) = claimed else {
            continue;
        };
        state.emit(
            "alarm",
            json!({"event": "ringing", "alarm_id": alarm.id, "label": alarm.label}),
        );
        deliver(state, alarm, delivery_id, occurrence).await;
    }
    Ok(())
}

/// Next occurrence of a recurring alarm strictly after `now`; `None` for
/// one-shot alarms (which are then done).
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
    let next = next_occurrence_after(alarm, now);
    let new_status = if next.is_some() { "scheduled" } else { "done" };
    let aid = alarm.id.clone();
    let did = delivery_id.to_string();
    let st = status.to_string();
    let det = detail.to_string();
    let _ = state
        .db
        .call(move |c| {
            let tx = c.transaction()?;
            tx.execute(
                "UPDATE alarm_deliveries SET status=?1, detail=?2, finished_at=?3 WHERE id=?4",
                rusqlite::params![st, det, now, did],
            )?;
            tx.execute(
                "UPDATE alarms SET status=?1, next_due_utc=?2, updated_at=?3,
                    schedule_version=schedule_version+1 WHERE id=?4",
                rusqlite::params![new_status, next, now, aid],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await;
    state.emit(
        "alarm",
        json!({
            "event": "delivered",
            "alarm_id": alarm.id,
            "status": status,
            "detail": detail,
            "occurrence_utc": occurrence,
        }),
    );
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
                    .get_setting("public_url")
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| state.config.base_url());
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
                    Ok((_, result)) => (
                        "played".into(),
                        format!("played on browser endpoint '{ep_name}' ({result}){late_note}"),
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

        reconcile_after_boot(state.as_ref()).await;

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
}
