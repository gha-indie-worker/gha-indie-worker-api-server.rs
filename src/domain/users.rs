#![forbid(unsafe_code)]

//! Individual (B2C) users and the `/v1/users/me` projection.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::onboarding::UserOnboardingState;
use super::orgs::{normalize_email, OrgError, OrgRole};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id: Uuid,
    /// The authenticated subject. Stable across shared-auth, Supabase and Neon
    /// because shared-auth federates all three onto one subject.
    pub subject: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub onboarding: UserOnboardingState,
    pub created_at: String,
}

impl User {
    /// Smart constructor from a verified subject.
    ///
    /// # Errors
    /// Returns [`OrgError::InvalidEmail`] when an email is supplied and is not
    /// a bounded `local@domain` pair.
    pub fn create(
        id: Uuid,
        subject: String,
        email: Option<&str>,
        created_at: String,
    ) -> Result<Self, OrgError> {
        let email = match email {
            None => None,
            Some(value) => Some(normalize_email(value)?),
        };
        Ok(Self {
            id,
            subject,
            email,
            display_name: None,
            onboarding: UserOnboardingState::Signup,
            created_at,
        })
    }
}

/// One organisation the caller belongs to, as returned by `/v1/users/me`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Membership {
    pub org_id: Uuid,
    pub org_slug: String,
    pub role: OrgRole,
}

/// The `/v1/users/me` response. Deliberately a *projection*: it carries no
/// token, no provider identifiers and no internal state beyond onboarding.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Me {
    pub id: Uuid,
    pub subject: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub onboarding: UserOnboardingState,
    pub auth_source: String,
    pub scopes: Vec<String>,
    pub memberships: Vec<Membership>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct UpdateMe {
    #[serde(default)]
    pub display_name: Option<String>,
}

/// # Errors
/// Returns [`OrgError::InvalidName`] when the display name is empty after
/// trimming or longer than 200 characters.
pub fn normalize_display_name(value: &str) -> Result<String, OrgError> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 200 {
        return Err(OrgError::InvalidName);
    }
    Ok(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_user_starts_at_signup_with_a_normalised_email() {
        let user = User::create(
            Uuid::nil(),
            "sub-123".to_owned(),
            Some("  Alex@Example.COM "),
            "2026-01-01T00:00:00Z".to_owned(),
        )
        .expect("valid user");
        assert_eq!(user.onboarding, UserOnboardingState::Signup);
        assert_eq!(user.email.as_deref(), Some("alex@example.com"));
        assert_eq!(user.display_name, None);
    }

    #[test]
    fn a_user_may_exist_without_an_email() {
        let user = User::create(Uuid::nil(), "sub".to_owned(), None, String::new())
            .expect("email is optional");
        assert_eq!(user.email, None);
    }

    #[test]
    fn an_unusable_email_is_rejected_at_construction() {
        assert_eq!(
            User::create(Uuid::nil(), "sub".to_owned(), Some("nope"), String::new()).map(|_| ()),
            Err(OrgError::InvalidEmail)
        );
    }

    #[test]
    fn display_names_are_trimmed_and_bounded() {
        assert_eq!(normalize_display_name("  Alex  ").as_deref(), Ok("Alex"));
        assert_eq!(normalize_display_name("   "), Err(OrgError::InvalidName));
        assert_eq!(
            normalize_display_name(&"x".repeat(201)),
            Err(OrgError::InvalidName)
        );
    }
}
