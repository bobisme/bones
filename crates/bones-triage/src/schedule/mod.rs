pub mod fallback;
pub mod whittle;

pub use fallback::{
    Assignment, FallbackConfig, ScheduleRegime, assign_fallback, assign_fallback_with_config,
};
pub use whittle::{
    IndexabilityResult, WhittleConfig, WhittleIndex, check_indexability, compute_whittle_indices,
};
