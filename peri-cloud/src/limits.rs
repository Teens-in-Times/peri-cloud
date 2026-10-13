//! Admission limits are host policy, independent of tool approval.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeLimits {
    pub max_active_turns: usize,
    pub max_active_per_user: usize,
    pub daily_turns: u32,
    pub daily_model_requests: u32,
    pub max_turn_seconds: u64,
    pub max_prompt_bytes: usize,
    pub max_user_data_bytes: usize,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            max_active_turns: 4,
            max_active_per_user: 1,
            daily_turns: 200,
            daily_model_requests: 1000,
            max_turn_seconds: 600,
            max_prompt_bytes: 65536,
            max_user_data_bytes: 8192,
        }
    }
}

impl RuntimeLimits {
    pub fn valid(&self) -> bool {
        (1..=128).contains(&self.max_active_turns)
            && (1..=self.max_active_turns).contains(&self.max_active_per_user)
            && (1..=100000).contains(&self.daily_turns)
            && (1..=1000000).contains(&self.daily_model_requests)
            && (1..=86400).contains(&self.max_turn_seconds)
            && (256..=1048576).contains(&self.max_prompt_bytes)
            && (256..=8192).contains(&self.max_user_data_bytes)
    }
}
