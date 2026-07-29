//! Deterministic global-concurrency sharding.

use std::collections::BTreeSet;

use thiserror::Error;

use crate::protocol::AgentShard;

/// Shard-planning failure.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShardError {
    /// At least one agent is required.
    #[error("at least one distributed agent is required")]
    EmptyAgents,
    /// Agent inventory names must be unique.
    #[error("duplicate distributed agent name `{0}`")]
    DuplicateAgent(String),
    /// Names are used in artifact paths and must be a small portable token.
    #[error("invalid distributed agent name `{0}`")]
    InvalidAgentName(String),
    /// Every active MVP agent must receive at least one local worker.
    #[error(
        "global concurrency {global} is smaller than active agent count {agents}; every agent must receive at least one worker"
    )]
    InsufficientConcurrency {
        /// Requested global concurrency.
        global: u32,
        /// Number of active agents.
        agents: usize,
    },
    /// The active agent count cannot fit in the wire representation.
    #[error("too many active agents")]
    TooManyAgents,
}

/// Split global concurrency by quotient/remainder after stable name sorting.
///
/// The first `global_concurrency % agent_count` names receive one additional
/// worker. Sorting makes the assignment independent of connection/ready
/// arrival order.
pub fn plan_shards(
    global_concurrency: u32,
    agent_names: &[String],
) -> Result<Vec<AgentShard>, ShardError> {
    if agent_names.is_empty() {
        return Err(ShardError::EmptyAgents);
    }
    let agent_count = u32::try_from(agent_names.len()).map_err(|_| ShardError::TooManyAgents)?;
    if global_concurrency < agent_count {
        return Err(ShardError::InsufficientConcurrency {
            global: global_concurrency,
            agents: agent_names.len(),
        });
    }

    let mut names = BTreeSet::new();
    for name in agent_names {
        if !is_portable_name(name) {
            return Err(ShardError::InvalidAgentName(name.clone()));
        }
        if !names.insert(name.clone()) {
            return Err(ShardError::DuplicateAgent(name.clone()));
        }
    }

    let base = global_concurrency / agent_count;
    let remainder = global_concurrency % agent_count;
    Ok(names
        .into_iter()
        .enumerate()
        .map(|(index, agent_name)| {
            let index = u32::try_from(index).expect("agent count was checked above");
            AgentShard {
                agent_name,
                index,
                agent_count,
                concurrency: base + u32::from(index < remainder),
            }
        })
        .collect())
}

fn is_portable_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && !matches!(value, "." | "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn quotient_remainder_is_stable_by_name() {
        let shards = plan_shards(8, &names(&["west", "east", "central"])).unwrap();
        let view: Vec<_> = shards
            .iter()
            .map(|shard| (shard.agent_name.as_str(), shard.concurrency))
            .collect();
        assert_eq!(view, vec![("central", 3), ("east", 3), ("west", 2)]);
        assert_eq!(shards.iter().map(|s| s.concurrency).sum::<u32>(), 8);
    }

    #[test]
    fn every_agent_gets_a_worker() {
        let shards = plan_shards(3, &names(&["a", "b", "c"])).unwrap();
        assert!(shards.iter().all(|shard| shard.concurrency == 1));
    }

    #[test]
    fn rejects_impossible_and_ambiguous_inventory() {
        assert_eq!(
            plan_shards(1, &names(&["a", "b"])).unwrap_err(),
            ShardError::InsufficientConcurrency {
                global: 1,
                agents: 2
            }
        );
        assert_eq!(
            plan_shards(2, &names(&["a", "a"])).unwrap_err(),
            ShardError::DuplicateAgent("a".to_owned())
        );
        assert!(matches!(
            plan_shards(2, &names(&["a", "../b"])),
            Err(ShardError::InvalidAgentName(_))
        ));
    }
}
