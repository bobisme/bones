#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TerseIdError {
    #[error("invalid ID format: {id}")]
    InvalidId { id: String },

    #[error("prefix mismatch: expected '{expected}', found '{found}'")]
    PrefixMismatch { expected: String, found: String },

    #[error("ambiguous ID '{partial}': matches {matches:?}")]
    AmbiguousId {
        partial: String,
        matches: Vec<String>,
    },

    #[error("ID not found: {id}")]
    NotFound { id: String },
}

pub type Result<T> = std::result::Result<T, TerseIdError>;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_invalid_id_display() {
        let error = TerseIdError::InvalidId {
            id: "bad-id".to_string(),
        };
        assert_eq!(error.to_string(), "invalid ID format: bad-id");
    }

    #[test]
    fn test_prefix_mismatch_display() {
        let error = TerseIdError::PrefixMismatch {
            expected: "usr".to_string(),
            found: "org".to_string(),
        };
        assert_eq!(
            error.to_string(),
            "prefix mismatch: expected 'usr', found 'org'"
        );
    }

    #[test]
    fn test_ambiguous_id_display() {
        let error = TerseIdError::AmbiguousId {
            partial: "usr_a".to_string(),
            matches: vec!["usr_abc123".to_string(), "usr_abd456".to_string()],
        };
        assert_eq!(
            error.to_string(),
            "ambiguous ID 'usr_a': matches [\"usr_abc123\", \"usr_abd456\"]"
        );
    }

    #[test]
    fn test_not_found_display() {
        let error = TerseIdError::NotFound {
            id: "usr_xyz789".to_string(),
        };
        assert_eq!(error.to_string(), "ID not found: usr_xyz789");
    }

    #[test]
    fn test_error_debug() {
        let error = TerseIdError::InvalidId {
            id: "test".to_string(),
        };
        assert!(format!("{error:?}").contains("InvalidId"));
    }

    #[test]
    fn test_error_clone() {
        let error1 = TerseIdError::NotFound {
            id: "test_id".to_string(),
        };
        let error2 = error1.clone();
        assert_eq!(error1, error2);
    }

    #[test]
    fn test_error_equality() {
        let error1 = TerseIdError::InvalidId {
            id: "same".to_string(),
        };
        let error2 = TerseIdError::InvalidId {
            id: "same".to_string(),
        };
        assert_eq!(error1, error2);
    }

    #[test]
    fn test_result_type_ok() {
        let result: Result<i32> = Ok(42);
        assert_eq!(result, Ok(42));
    }

    #[test]
    fn test_result_type_err() {
        let error = TerseIdError::NotFound {
            id: "test".to_string(),
        };
        let result: Result<i32> = Err(error.clone());
        assert_eq!(result, Err(error));
    }
}
