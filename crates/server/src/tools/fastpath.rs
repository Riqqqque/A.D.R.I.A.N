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

/// Polite words allowed in front of a command, and only there.
const POLITE: [&str; 6] = ["please", "can", "could", "would", "will", "you"];
/// Words that don't tell one light from another.
const FILLERS: [&str; 8] = ["the", "my", "in", "a", "an", "light", "lights", "lamp"];

/// Lowercased words, without the sentence punctuation speech-to-text adds.
fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| w.trim_end_matches(['.', ',', '!']).to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

/// What identifies a spoken target, name or area once the fillers are gone.
fn key<S: AsRef<str>>(words: &[S]) -> Vec<String> {
    words
        .iter()
        .map(|w| w.as_ref().to_string())
        .filter(|w| !FILLERS.contains(&w.as_str()))
        .collect()
}

/// Read `text` as exactly one command form: `turn|switch on|off <target>`,
/// `turn|switch <target> on|off`, `toggle <target>` or
/// `<target> light|lights|lamp on|off`. Returns the action and the words that
/// name the target. Anything else is None, questions included: a sentence
/// that only contains a command is the model's to read.
fn command(text: &str) -> Option<(&'static str, Vec<String>)> {
    if text.contains('?') {
        return None;
    }
    let all = words(text);
    let start = all.iter().position(|w| !POLITE.contains(&w.as_str()))?;
    let spoken: Vec<&str> = all[start..].iter().map(String::as_str).collect();
    let (switch, target) = match spoken.as_slice() {
        ["toggle", target @ ..] => (None, target),
        ["turn" | "switch", s @ ("on" | "off"), target @ ..] => (Some(*s), target),
        ["turn" | "switch", target @ .., s @ ("on" | "off")] => (Some(*s), target),
        [target @ .., "light" | "lights" | "lamp", s @ ("on" | "off")] => (Some(*s), target),
        _ => return None,
    };
    if !spoken
        .iter()
        .any(|w| w.contains("light") || w.contains("lamp"))
    {
        return None;
    }
    let action = match switch {
        None => "toggle",
        Some("on") => "turn_on",
        Some(_) => "turn_off",
    };
    Some((action, key(target)))
}

/// Try to interpret `text` as a simple light command against allowlisted
/// entities. Returns None unless it parses exactly and names one light by its
/// whole name or area — the LLM path then handles it.
pub async fn parse(state: &Arc<AppState>, text: &str) -> Result<Option<FastPath>, ApiError> {
    let lower = text.to_lowercase();
    let trimmed = lower.trim();
    if trimmed == "what time is it" || trimmed == "what time is it?" {
        return Ok(Some(FastPath::CurrentTime));
    }
    let Some((action, target)) = command(trimmed) else {
        return Ok(None);
    };
    let lights = state
        .db
        .q(
            "SELECT entity_id, name, area FROM entities
             WHERE allowed = 1 AND domain = 'light' AND state <> 'removed'",
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
    let matches: Vec<_> = lights
        .iter()
        .filter(|(_, name, area)| {
            if target.is_empty() {
                lights.len() == 1
            } else {
                key(&words(name)) == target
                    || area.as_deref().is_some_and(|a| key(&words(a)) == target)
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
            trusted_proxy: None,
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

    /// Lights whose names and areas overlap the words people actually say.
    async fn state_with_house() -> (Arc<AppState>, tempfile::TempDir) {
        let (state, dir) = state_with_light().await;
        state
            .db
            .exec(
                "UPDATE entities SET allowed = 1 WHERE entity_id = 'light.hall'",
                vec![],
            )
            .await
            .unwrap();
        state
            .db
            .exec(
                "INSERT INTO entities(entity_id,name,area,domain,state,allowed,updated_at)
                 VALUES('light.porch','Porch',NULL,'light','off',1,1),
                       ('light.ceiling','Ceiling','Porch','light','off',1,1),
                       ('light.pantry','Pantry',NULL,'light','off',1,1),
                       ('light.spot','Spot','Den','light','off',1,1)",
                vec![],
            )
            .await
            .unwrap();
        (state, dir)
    }

    #[tokio::test]
    async fn only_a_whole_command_for_a_whole_name_acts() {
        let (state, _d) = state_with_house().await;
        let mut wrong = Vec::new();
        for text in [
            "turn off all the lights",
            "turn off all the lights.",
            "is the porch light on?",
            "is the porch light on",
            "is the pantry light on?",
            "is the pantry light on",
            "don't turn off the kitchen light",
            "don't turn off the pantry light",
            "remind me to turn off the porch light at 10",
            "remind me to turn off the pantry light at 10",
            "turn off the kitchen light and the hall light",
            "turn off every light except the hall light",
            "turn off the kitchen light?",
            "what happens if you turn off the kitchen light",
            "turn off the kit light",
            "turn off the lights",
            "turn off",
            "please",
        ] {
            let got = parse(&state, text).await.unwrap();
            if got.is_some() {
                wrong.push(format!("{text:?} acted: {got:?}"));
            }
        }
        for (text, id, want) in [
            ("turn off the kitchen light", "light.kitchen", "turn_off"),
            ("Turn off the kitchen light.", "light.kitchen", "turn_off"),
            (
                "please turn off the kitchen light",
                "light.kitchen",
                "turn_off",
            ),
            (
                "could you turn the hall light off",
                "light.hall",
                "turn_off",
            ),
            (
                "can you please switch on the hall lamp!",
                "light.hall",
                "turn_on",
            ),
            ("toggle the hall light", "light.hall", "toggle"),
            ("kitchen light off", "light.kitchen", "turn_off"),
            ("the kitchen lights on", "light.kitchen", "turn_on"),
            ("switch the ceiling light on", "light.ceiling", "turn_on"),
            ("turn off the light in the den", "light.spot", "turn_off"),
        ] {
            match parse(&state, text).await.unwrap() {
                Some(FastPath::LightCommand {
                    entity_id, action, ..
                }) if entity_id == id && action == want => {}
                other => wrong.push(format!("{text:?} gave {other:?}")),
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[tokio::test]
    async fn the_lights_means_the_only_light_there_is() {
        let (state, _d) = state_with_light().await;
        for text in ["turn off the lights", "lights off", "turn the light off"] {
            assert!(
                matches!(
                    parse(&state, text).await.unwrap(),
                    Some(FastPath::LightCommand { .. })
                ),
                "{text}"
            );
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
