use std::collections::{HashMap, HashSet};

/// Default number of consecutive automatic failures before suspension.
pub const MAX_AUTO_FAILURES: u32 = 5;

/// Per-device automatic refresh suspension policy.
#[derive(Debug, Clone)]
pub struct FailurePolicy {
    max_failures: u32,
    streaks: HashMap<String, u32>,
    suspended: HashSet<String>,
}

impl Default for FailurePolicy {
    fn default() -> Self {
        Self::new(MAX_AUTO_FAILURES)
    }
}

impl FailurePolicy {
    /// Creates a policy with a suspension threshold of at least one.
    pub fn new(max_failures: u32) -> Self {
        Self {
            max_failures: max_failures.max(1),
            streaks: HashMap::new(),
            suspended: HashSet::new(),
        }
    }

    /// Resets every failure counter and suspension.
    pub fn reset(&mut self) {
        self.streaks.clear();
        self.suspended.clear();
    }

    /// Returns whether a device is suspended from automatic refreshes.
    pub fn is_suspended(&self, key: &str) -> bool {
        self.suspended.contains(key)
    }

    /// Returns the current consecutive failure count.
    pub fn streak(&self, key: &str) -> u32 {
        self.streaks.get(key).copied().unwrap_or_default()
    }

    /// Returns a snapshot of suspended device keys.
    pub fn suspended(&self) -> HashSet<String> {
        self.suspended.clone()
    }

    /// Filters suspended keys while preserving input order.
    pub fn auto_keys<'a>(&self, keys: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        keys.into_iter()
            .filter(|key| !self.is_suspended(key))
            .map(str::to_owned)
            .collect()
    }

    /// Records an automatic result and returns true only on new suspension.
    pub fn record(&mut self, key: &str, ok: bool) -> bool {
        if ok {
            self.streaks.remove(key);
            self.suspended.remove(key);
            return false;
        }
        let current = self.streaks.get(key).copied().unwrap_or_default() + 1;
        self.streaks.insert(key.to_owned(), current);
        if current >= self.max_failures && !self.suspended.contains(key) {
            self.suspended.insert(key.to_owned());
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifth_failure_suspends_and_success_recovers() {
        let mut policy = FailurePolicy::default();
        for expected in 1..MAX_AUTO_FAILURES {
            assert!(!policy.record("headphone", false));
            assert_eq!(policy.streak("headphone"), expected);
        }
        assert!(policy.record("headphone", false));
        assert!(policy.is_suspended("headphone"));
        assert!(!policy.record("headphone", false));
        assert!(!policy.record("headphone", true));
        assert!(!policy.is_suspended("headphone"));
        assert_eq!(policy.streak("headphone"), 0);
    }

    #[test]
    fn filtering_is_ordered_and_reset_is_complete() {
        let mut policy = FailurePolicy::new(1);
        assert!(policy.record("b", false));
        assert_eq!(policy.auto_keys(["a", "b", "c"]), ["a", "c"]);
        policy.reset();
        assert!(policy.suspended().is_empty());
        assert_eq!(policy.auto_keys(["a", "b"]), ["a", "b"]);
    }
}
