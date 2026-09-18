//! Argon2id password hashing (PHC string format).

use argon2::{Argon2, PasswordHasher, PasswordVerifier};

pub fn hash_password(password: &str) -> Result<String, crate::error::ApiError> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| crate::error::ApiError::internal_msg(format!("argon2: {e}")))
}

pub fn verify_password(stored_phc: &str, password: &str) -> bool {
    Argon2::default()
        .verify_password(password.as_bytes(), stored_phc)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_roundtrips_and_rejects_wrong_password() {
        let phc = hash_password("correct horse battery").unwrap();
        assert!(phc.starts_with("$argon2"));
        assert!(verify_password(&phc, "correct horse battery"));
        assert!(!verify_password(&phc, "correct horse batter"));
    }

    #[test]
    fn hashes_are_salted() {
        assert_ne!(
            hash_password("same").unwrap(),
            hash_password("same").unwrap()
        );
    }
}
