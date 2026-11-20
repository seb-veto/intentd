//! Wire-policy glue for the read-only `sentry.*` methods (PROTOCOL §5.29).
//!
//! The engine and REST transport live in `intent-sentry`. This module
//! resolves server-side credentials and maps engine errors onto the wire.
//! Only derived identity crosses the wire.

use std::sync::Arc;

use intent_core::{Error, Result};
use intent_sentry::{IssueStatusFilter, SentryEngine, SentryRegistry, SentrySettings};

/// Map a Sentry engine/registry error onto the domain `Internal` error
/// (→ `-32603`): a missing/invalid credential pair (`NotConfigured`) and any
/// other Sentry failure both surface as `Internal` with a descriptive
/// message (§5.29, §9).
pub(crate) fn map_sentry_err(e: &intent_sentry::Error) -> Error {
    Error::Internal(e.to_string())
}

pub(crate) const SENTRY_TOKEN_SETTING: &str = "accounts.sentry.token";
pub(crate) const SENTRY_ORGANIZATION_SETTING: &str = "accounts.sentry.organization";

impl crate::Services {
    pub(super) async fn resolve_sentry_engine(&self) -> Result<Arc<dyn SentryEngine>> {
        if let Some(engine) = &self.sentry_engine {
            return Ok(Arc::clone(engine));
        }

        SentryRegistry::from_settings(&self.sentry_settings().await?)
            .await
            .map_err(|e| map_sentry_err(&e))
    }

    async fn sentry_settings(&self) -> Result<SentrySettings> {
        // Match settings.get lock ordering. Read both halves of the saved
        // account without racing a settings.update batch or its rollback.
        let _secret_guard = self
            .settings_secret_gates
            .read(Some(SENTRY_TOKEN_SETTING))
            .await;
        let _revision_guard = self.settings_revision_gate.read().await;
        let setting = self
            .settings_service()
            .completion_value(SENTRY_ORGANIZATION_SETTING)
            .await?;
        let Some(organization) = setting["value"]
            .as_str()
            .filter(|org| !org.trim().is_empty())
        else {
            return Ok(SentrySettings::default());
        };

        Ok(SentrySettings {
            organization: Some(organization.to_owned()),
            token: self.secrets.load(SENTRY_TOKEN_SETTING).await?,
            ..SentrySettings::default()
        })
    }
}

/// Validate/default the `sentry.listIssues` `status` (default `unresolved`);
/// an invalid value is rejected with `InvalidParams` (→ `-32602`, §5.29).
/// `None` returns `None` so the engine applies its own default policy
/// (matches the FE which omits the field).
pub(crate) fn parse_status(status: Option<&str>) -> Result<Option<IssueStatusFilter>> {
    match status {
        None => Ok(None),
        Some("unresolved") => Ok(Some(IssueStatusFilter::Unresolved)),
        Some("resolved") => Ok(Some(IssueStatusFilter::Resolved)),
        Some("ignored") => Ok(Some(IssueStatusFilter::Ignored)),
        Some("all") => Ok(Some(IssueStatusFilter::All)),
        Some(_) => Err(Error::InvalidParams(
            "status must be one of: unresolved, resolved, ignored, all".to_string(),
        )),
    }
}

/// Narrow the wire `limit` (`i64`) to the engine's `Option<u32>`; non-positive
/// or out-of-range values fall through to the engine's own default/clamp.
pub(crate) fn wire_limit(limit: Option<i64>) -> Option<u32> {
    limit.and_then(|n| u32::try_from(n).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[intent_test_macros::daemon_test]
    async fn engine_uses_credentials_saved_by_settings() {
        use intent_core::WorkspaceApi;
        use serde_json::json;

        let temp = tempfile::tempdir().unwrap();
        let store = intent_store::Store::open(&temp.path().join("intentd.db"))
            .await
            .unwrap();
        let registry = crate::SettingsRegistry::load(temp.path().join("config.toml")).unwrap();
        let services = crate::Services::new_isolated(store)
            .with_settings_registry(Arc::new(registry))
            .with_secret_store(Arc::new(crate::settings::InMemorySecretStore::default()));

        services
            .settings_update(json!([
                {"path": SENTRY_TOKEN_SETTING, "value": "fixture-token"},
                {"path": SENTRY_ORGANIZATION_SETTING, "value": "acme"}
            ]))
            .await
            .unwrap();
        let settings = services.sentry_settings().await.unwrap();
        assert_eq!(settings.organization.as_deref(), Some("acme"));
        assert_eq!(settings.token.as_deref(), Some("fixture-token"));
        assert!(services.resolve_sentry_engine().await.is_ok());

        services
            .settings_reset(SENTRY_TOKEN_SETTING.to_owned())
            .await
            .unwrap();
        assert!(services.sentry_settings().await.unwrap().token.is_none());
    }

    #[test]
    fn parses_status_with_default() {
        assert_eq!(parse_status(None).unwrap(), None);
        assert_eq!(
            parse_status(Some("unresolved")).unwrap(),
            Some(IssueStatusFilter::Unresolved)
        );
        assert_eq!(
            parse_status(Some("resolved")).unwrap(),
            Some(IssueStatusFilter::Resolved)
        );
        assert_eq!(
            parse_status(Some("ignored")).unwrap(),
            Some(IssueStatusFilter::Ignored)
        );
        assert_eq!(
            parse_status(Some("all")).unwrap(),
            Some(IssueStatusFilter::All)
        );
    }

    #[test]
    fn rejects_invalid_status() {
        assert!(matches!(
            parse_status(Some("bogus")),
            Err(Error::InvalidParams(_))
        ));
    }

    #[test]
    fn narrows_wire_limit() {
        assert_eq!(wire_limit(None), None);
        assert_eq!(wire_limit(Some(50)), Some(50));
        assert_eq!(wire_limit(Some(-1)), None);
    }

    #[test]
    fn maps_not_configured_to_internal() {
        let mapped = map_sentry_err(&intent_sentry::Error::NotConfigured("no creds".into()));
        assert!(matches!(mapped, Error::Internal(_)));
    }
}
