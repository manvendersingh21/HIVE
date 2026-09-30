//! In-process local-time scheduling. SQLite remembers each day's attempt;
//! missed time on restart or a DST jump is caught up once that day.
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{Local, NaiveDateTime, NaiveTime};
use hive_core::memory::ingest::{IngestCounts, Ingestor};

use crate::chat::AgentHandle;

pub async fn ingest(State(handle): State<AgentHandle>) -> Response {
    let agent = match handle.require() {
        Ok(a) => a.clone(),
        Err(r) => return r,
    };
    // The server owns the operation even if the HTTP caller disconnects.
    match tokio::spawn(async move {
        agent
            .memory
            .ingestor
            .ingest(Local::now().date_naive())
            .await
    })
    .await
    {
        Ok(Ok(counts)) => Json(counts).into_response(),
        result => {
            tracing::warn!(?result, "manual memory ingestion failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Memory ingestion failed; pending sources were retained.",
            )
                .into_response()
        }
    }
}

fn schedule(time: &str) -> anyhow::Result<NaiveTime> {
    anyhow::ensure!(
        time.len() == 5 && time.as_bytes()[2] == b':',
        "memory.nightly.time must be HH:MM"
    );
    Ok(NaiveTime::parse_from_str(time, "%H:%M")?)
}

async fn tick(
    ingestor: &Ingestor,
    at: NaiveTime,
    now: NaiveDateTime,
) -> anyhow::Result<Option<IngestCounts>> {
    if !ingestor.nightly_config().enabled
        || now.time() < at
        || !ingestor.claim_nightly(now.date())?
    {
        return Ok(None);
    }
    ingestor.ingest(now.date()).await.map(Some)
}

pub fn start(handle: &AgentHandle) {
    let Some(agent) = &handle.agent else {
        return;
    };
    let ingestor = agent.memory.ingestor.clone();
    if !ingestor.nightly_config().enabled {
        return;
    }
    let at = match schedule(&ingestor.nightly_config().time) {
        Ok(at) => at,
        Err(error) => {
            tracing::warn!(%error, "nightly memory scheduler disabled");
            return;
        }
    };
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(30));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            match tick(&ingestor, at, Local::now().naive_local()).await {
                Ok(Some(counts)) => tracing::info!(?counts, "nightly memory ingestion finished"),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "nightly memory ingestion failed; sources remain pending")
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use hive_core::memory::MemorySystem;

    #[tokio::test]
    async fn nightly_time_catches_up_and_never_repeats_a_local_date() {
        let memory = MemorySystem::new();
        let date = chrono::NaiveDate::from_ymd_opt(2026, 10, 25).unwrap();
        let at = schedule("03:00").unwrap();
        assert!(
            tick(&memory.ingestor, at, date.and_hms_opt(2, 59, 59).unwrap())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            tick(&memory.ingestor, at, date.and_hms_opt(3, 0, 0).unwrap())
                .await
                .unwrap()
                .unwrap()
                .lessons,
            1
        );
        assert!(
            tick(&memory.ingestor, at, date.and_hms_opt(3, 0, 0).unwrap())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            tick(&memory.ingestor, at, date.and_hms_opt(12, 0, 0).unwrap())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            tick(
                &memory.ingestor,
                at,
                date.succ_opt().unwrap().and_hms_opt(7, 0, 0).unwrap()
            )
            .await
            .unwrap()
            .unwrap()
            .lessons,
            1
        );
        assert_eq!(memory.graph.recent_lessons(5).unwrap().len(), 2);
    }

    #[test]
    fn nightly_schedule_requires_a_valid_hh_mm_time() {
        for bad in ["3:00", "24:00", "03:60", "03:00Z", "midnight", "１２:００"] {
            assert!(schedule(bad).is_err(), "{bad}");
        }
        assert_eq!(
            schedule("23:59").unwrap(),
            NaiveTime::from_hms_opt(23, 59, 0).unwrap()
        );
    }
}
