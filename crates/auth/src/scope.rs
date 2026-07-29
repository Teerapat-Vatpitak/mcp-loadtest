//! OAuth scope selection and bounded step-up handling.

use std::collections::BTreeSet;
use std::fmt;

use crate::{AuthError, AuthResult};

/// A deterministic set of OAuth scope tokens.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ScopeSet(BTreeSet<String>);

impl ScopeSet {
    /// Create a scope set from whitespace-separated OAuth scope text.
    pub fn parse(value: &str) -> Self {
        Self(
            value
                .split_ascii_whitespace()
                .filter(|scope| !scope.is_empty())
                .map(str::to_owned)
                .collect(),
        )
    }

    /// Create a scope set from individual scope tokens.
    pub fn from_tokens(tokens: impl IntoIterator<Item = String>) -> Self {
        Self(
            tokens
                .into_iter()
                .filter(|scope| !scope.is_empty() && !scope.chars().any(char::is_whitespace))
                .collect(),
        )
    }

    /// Return whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Return whether a scope is present.
    pub fn contains(&self, scope: &str) -> bool {
        self.0.contains(scope)
    }

    /// Add a valid single scope token.
    pub fn insert(&mut self, scope: impl Into<String>) {
        let scope = scope.into();
        if !scope.is_empty() && !scope.chars().any(char::is_whitespace) {
            self.0.insert(scope);
        }
    }

    /// Return the deterministic union of two sets.
    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        Self(self.0.union(&other.0).cloned().collect())
    }

    /// Return a space-separated value suitable for OAuth requests.
    pub fn to_oauth_string(&self) -> String {
        self.0.iter().cloned().collect::<Vec<_>>().join(" ")
    }

    /// Iterate over scope tokens.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }
}

impl fmt::Debug for ScopeSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_set().entries(self.0.iter()).finish()
    }
}

/// Tracks a bounded number of authorization retries for incremental consent.
#[derive(Debug, Clone)]
pub struct StepUpTracker {
    attempts: usize,
    maximum_attempts: usize,
}

impl StepUpTracker {
    /// Create a tracker with an explicit retry limit.
    pub fn new(maximum_attempts: usize) -> Self {
        Self {
            attempts: 0,
            maximum_attempts,
        }
    }

    /// Merge prior and challenged scopes, consuming one retry allowance.
    pub fn next(&mut self, prior: &ScopeSet, challenged: &ScopeSet) -> AuthResult<ScopeSet> {
        if self.attempts >= self.maximum_attempts {
            return Err(AuthError::StepUpRetryLimit);
        }
        self.attempts += 1;
        Ok(prior.union(challenged))
    }

    /// Return the number of retries already consumed.
    pub fn attempts(&self) -> usize {
        self.attempts
    }
}

impl Default for StepUpTracker {
    fn default() -> Self {
        Self::new(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_union_is_deterministic_and_deduplicated() {
        let prior = ScopeSet::parse("mcp:read mcp:basic");
        let challenged = ScopeSet::parse("mcp:write mcp:read");
        assert_eq!(
            prior.union(&challenged).to_oauth_string(),
            "mcp:basic mcp:read mcp:write"
        );
    }

    #[test]
    fn retries_are_bounded() {
        let scopes = ScopeSet::parse("mcp:read");
        let mut tracker = StepUpTracker::new(1);
        assert!(tracker.next(&scopes, &scopes).is_ok());
        assert_eq!(
            tracker.next(&scopes, &scopes),
            Err(AuthError::StepUpRetryLimit)
        );
    }
}
