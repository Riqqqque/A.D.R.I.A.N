//! Deterministic fast path: unambiguous known commands skip the LLM but run
//! through the exact same policy checks as tool calls.

use crate::error::ApiError;
use crate::state::AppState;
use std::sync::Arc;

#[derive(Debug)]
pub enum FastPath {
    LightCommand {
        entity_id: String,
        name: String,
        action: String,
    },
    CurrentTime,
}

/// Try to interpret `text` as a simple light command against allowlisted
/// entities. Returns None on ambiguity — the LLM path then handles it.
pub async fn parse(state: &Arc<AppState>, text: &str) -> Result<Option<FastPath>, ApiError> {
    let lower = text.to_lowercase();
    let trimmed = lower.trim();
    if trimmed == "what time is it" || trimmed == "what time is it?" {
        return Ok(Some(FastPath::CurrentTime));
    }
    let action = if trimmed.contains("turn off")
        || trimmed.contains("switch off")
        || trimmed.contains("lights off")
        || trimmed.contains("light off")
    {
        "turn_off"
    } else if trimmed.contains("turn on")
        || trimmed.contains("switch on")
        || trimmed.contains("lights on")
        || trimmed.contains("light on")
    {
        "turn_on"
    } else if trimmed.contains("toggle") {
        "toggle"
    } else {
        return Ok(None);
    };
    if !(trimmed.contains("light") || trimmed.contains("lamp")) {
        return Ok(None);
    }
    let lights = state
        .db
        .q(
            "SELECT entity_id, name, area FROM entities
             WHERE allowed = 1 AND domain = 'light'",
            vec![],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .await?;
    // Strip command words; whatever remains should name a light or area.
    let mut residue = trimmed.to_string();
    for w in [
        "please", "can", "could", "you", "turn", "switch", "toggle", "on", "off", "the", "a", "an",
        "light", "lights", "lamp", "in", "my",
    ] {
        residue = residue
            .split_whitespace()
            .filter(|t| *t != w)
            .collect::<Vec<_>>()
            .join(" ");
    }
    let residue = residue.trim().to_string();
    let matches: Vec<_> = lights
        .iter()
        .filter(|(id, name, area)| {
            if residue.is_empty() {
                lights.len() == 1
                    || area
                        .as_deref()
                        .map(|a| trimmed.contains(&a.to_lowercase()))
                        .unwrap_or(false)
            } else {
                name.to_lowercase().contains(&residue)
                    || residue.contains(&name.to_lowercase())
                    || area
                        .as_deref()
                        .map(|a| residue.contains(&a.to_lowercase()))
                        .unwrap_or(false)
                    || id.to_lowercase().contains(&residue)
            }
        })
        .collect();
    if matches.len() == 1 {
        let (id, name, _) = matches[0];
        return Ok(Some(FastPath::LightCommand {
            entity_id: id.clone(),
            name: name.clone(),
            action: action.to_string(),
        }));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn state_with_light() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::Config {
            data_dir: dir.path().to_path_buf(),
            bind: "127.0.0.1:0".parse().unwrap(),
            web_dir: dir.path().to_path_buf(),
            tls: None,
        };
        let state = crate::build_state(config).await.unwrap();
        state
            .db
            .exec(
                "INSERT INTO entities(entity_id,name,domain,state,allowed,updated_at)
                 VALUES('light.kitchen','Kitchen Light','light','off',1,1),
                       ('light.hall','Hall Light','light','off',0,1)",
                vec![],
            )
            .await
            .unwrap();
        (state, dir)
    }

    #[tokio::test]
    async fn matches_allowlisted_light_and_action() {
        let (state, _d) = state_with_light().await;
        match parse(&state, "turn off the kitchen light").await.unwrap() {
            Some(FastPath::LightCommand {
                entity_id, action, ..
            }) => {
                assert_eq!(entity_id, "light.kitchen");
                assert_eq!(action, "turn_off");
            }
            other => panic!("expected light command, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ignores_unallowlisted_and_non_light_text() {
        let (state, _d) = state_with_light().await;
        // Hall light is not allowlisted — no fast path.
        assert!(
            parse(&state, "turn on the hall light")
                .await
                .unwrap()
                .is_none()
        );
        assert!(parse(&state, "tell me a joke").await.unwrap().is_none());
        assert!(matches!(
            parse(&state, "what time is it").await.unwrap(),
            Some(FastPath::CurrentTime)
        ));
    }
}
