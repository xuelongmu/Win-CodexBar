use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use uuid::Uuid;

use codexbar::codex_accounts::{
    AccountStore, CodexAccount, CodexAccountApi, CodexAccountManager, CodexAccountManagerError,
    CodexApiError, CodexSwitchResult, SnapshotStore, restart_codex_desktop,
};

use crate::state::AppState;

use super::*;

// ── Codex multi-account (ADR 0003, milestone 2) ──────────────────────

const DEFAULT_FETCH_TIMEOUT_SECONDS: u64 = 60;
static ACCOUNT_MUTATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static PENDING_RESTART: Mutex<Option<CodexSwitchResult>> = Mutex::new(None);

/// All stored + discovered Codex accounts, with the stored list preferred.
pub(crate) fn load_codex_accounts() -> Result<Vec<CodexAccount>, String> {
    let store = AccountStore::new();
    let existing = store.load_accounts().map_err(|e| e.to_string())?;

    let manager = CodexAccountManager::new();
    let managed = manager
        .discover_managed_accounts(&existing)
        .map_err(|e| e.to_string())?;
    let ambient = manager.discover_ambient_account(&existing);

    Ok(reconcile_codex_accounts(&existing, &managed, ambient))
}

fn reconcile_codex_accounts(
    existing: &[CodexAccount],
    managed: &[CodexAccount],
    ambient: Option<CodexAccount>,
) -> Vec<CodexAccount> {
    let mut merged: Vec<CodexAccount> = managed.to_vec();
    if let Some(ambient) = ambient {
        if let Some(entry) = merged.iter_mut().find(|account| account.matches(&ambient)) {
            entry.merge_from(&ambient);
        } else {
            merged.push(ambient);
        }
    }

    // Reconcile persisted metadata (nickname, stored timestamps) for managed homes.
    let mut reconciled: Vec<CodexAccount> = existing
        .iter()
        // The ambient home can change identity outside this app. Always use
        // its fresh discovery instead of retaining an old record for that path.
        .filter(|account| account.source.owns_files())
        // A managed home can also be reauthenticated outside the app. Do not
        // retain its former identity alongside the account now owning its auth.
        .filter(|account| {
            !managed.iter().any(|fresh| {
                fresh.standardized_home_path() == account.standardized_home_path()
                    && !fresh.matches(account)
            })
        })
        .map(|account| {
            let mut account = account.clone();
            if let Some(fresh) = managed.iter().find(|fresh| fresh.matches(&account)) {
                account.merge_from(fresh);
            }
            account
        })
        .collect();

    // Add any newly discovered accounts that are not yet persisted.
    for candidate in &merged {
        if !reconciled.iter().any(|account| account.matches(candidate)) {
            reconciled.push(candidate.clone());
        }
    }

    reconciled
}

/// Persist the given accounts to the account store.
pub(crate) fn persist_codex_accounts(accounts: &[CodexAccount]) -> Result<(), String> {
    let store = AccountStore::new();
    let (_existing, removed) = store.load().map_err(|e| e.to_string())?;
    store
        .save(accounts, Some(&removed))
        .map_err(|e| e.to_string())
}

/// Refresh quota snapshots for every Codex account (ADR 0003 multi-account
/// lanes).
///
/// Runs on the same refresh cycle as the ambient Codex provider lane: each
/// account (ambient + managed) is fetched concurrently, bounded by the shared
/// provider fetch semaphore, and persisted to the account snapshot store. A
/// `codex-accounts-updated` event lets surfaces (Settings accounts panel)
/// re-read the store without manual fetch.
///
/// Failures are per-account and non-fatal: the ambient provider snapshot and
/// the on-demand `codex_account_fetch` command remain authoritative, and the
/// store keeps the last good snapshot per account.
pub(crate) async fn refresh_codex_account_lanes(
    app: tauri::AppHandle,
    fetch_permits: Arc<tokio::sync::Semaphore>,
) {
    let accounts = match load_codex_accounts() {
        Ok(accounts) => accounts,
        Err(e) => {
            tracing::warn!("codex account lanes: failed to load accounts: {e}");
            return;
        }
    };
    if accounts.is_empty() {
        return;
    }

    let mut handles = Vec::with_capacity(accounts.len());
    for account in accounts {
        let permits = Arc::clone(&fetch_permits);
        handles.push(tokio::spawn(async move {
            let Ok(_permit) = permits.acquire_owned().await else {
                return None;
            };
            let api = CodexAccountApi::new();
            let home_path = account.codex_home_path.clone();
            let email_hint = account.email_hint.clone();
            match tokio::time::timeout(
                std::time::Duration::from_secs(DEFAULT_FETCH_TIMEOUT_SECONDS),
                api.fetch_snapshot(&home_path, email_hint.as_deref(), true),
            )
            .await
            {
                Ok(Ok(snapshot)) => Some((account.id, snapshot)),
                Ok(Err(e)) => {
                    tracing::debug!(
                        "codex account lane {} failed: {}",
                        account.id,
                        into_api_message(e)
                    );
                    None
                }
                Err(_) => {
                    tracing::debug!("codex account lane {} timed out", account.id);
                    None
                }
            }
        }));
    }

    let mut snapshots = SnapshotStore::new().load().unwrap_or_default();
    for handle in handles {
        if let Ok(Some((id, snapshot))) = handle.await {
            snapshots.insert(id, snapshot);
        }
    }
    if let Err(e) = SnapshotStore::new().save(&snapshots) {
        tracing::warn!("codex account lanes: failed to persist snapshots: {e}");
    }
    events::emit_codex_accounts_updated(&app);
}

#[tauri::command]
pub fn codex_accounts_list() -> Result<Vec<CodexAccount>, String> {
    load_codex_accounts()
}

#[tauri::command]
pub async fn codex_account_add(app: tauri::AppHandle) -> Result<CodexAccount, String> {
    let _mutation = ACCOUNT_MUTATION
        .try_lock()
        .map_err(|_| "A Codex account operation is already in progress.".to_string())?;
    let manager = CodexAccountManager::new();
    let account = tauri::async_runtime::spawn_blocking(move || manager.add_managed_account(None))
        .await
        .map_err(|e| e.to_string())?
        .map_err(into_user_message)?;

    if let Err(e) = refresh_persisted_accounts(app) {
        tracing::error!("failed to persist accounts after add: {e}");
    }
    Ok(account)
}

#[tauri::command]
pub fn codex_account_remove(app: tauri::AppHandle, id: String) -> Result<(), String> {
    let _mutation = ACCOUNT_MUTATION
        .try_lock()
        .map_err(|_| "A Codex account operation is already in progress.".to_string())?;
    let manager = CodexAccountManager::new();
    let accounts = load_codex_accounts()?;
    let target = accounts
        .iter()
        .find(|account| account.id.to_string() == id)
        .ok_or_else(|| "Codex account not found.".to_string())?;

    manager
        .remove_managed_files_if_owned(target)
        .map_err(into_user_message)?;

    let remaining: Vec<CodexAccount> = accounts
        .into_iter()
        .filter(|account| account.id.to_string() != id)
        .collect();
    persist_codex_accounts(&remaining)?;
    events::emit_settings_changed(&app);
    accounts_changed(&app);
    Ok(())
}

#[tauri::command]
pub async fn codex_account_switch(
    app: tauri::AppHandle,
    id: String,
) -> Result<CodexSwitchResult, String> {
    let _mutation = ACCOUNT_MUTATION
        .try_lock()
        .map_err(|_| "A Codex account operation is already in progress.".to_string())?;
    let manager = CodexAccountManager::new();
    let accounts = load_codex_accounts()?;
    let target = accounts
        .iter()
        .find(|account| account.id.to_string() == id)
        .ok_or_else(|| "Codex account not found.".to_string())?
        .clone();

    let persisted = accounts.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        manager.switch_active_account(&target, &persisted)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(into_user_message)?;

    // Materialized ambient account may need persisting.
    *PENDING_RESTART.lock().map_err(|e| e.to_string())? = Some(result.clone());
    let pending = {
        let state = app.state::<Mutex<AppState>>();
        let mut state = state.lock().map_err(|e| e.to_string())?;
        invalidate_account_usage(&mut state, ProviderId::Codex)
    };
    events::emit_provider_updated(&app, &pending);
    let refresh_app = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = do_refresh_providers(&refresh_app).await;
    });
    if let Some(materialized) = &result.materialized_account {
        let mut accounts = load_codex_accounts()?;
        if let Some(entry) = accounts.iter_mut().find(|a| a.matches(materialized)) {
            entry.merge_from(materialized);
        } else {
            accounts.push(materialized.clone());
        }
        persist_codex_accounts(&accounts)?;
    }

    events::emit_settings_changed(&app);
    accounts_changed(&app);
    Ok(result)
}

#[tauri::command]
pub async fn codex_account_fetch(
    app: tauri::AppHandle,
    id: String,
) -> Result<codexbar::codex_accounts::AccountUsageSnapshot, String> {
    let accounts = load_codex_accounts()?;
    let target = accounts
        .iter()
        .find(|account| account.id.to_string() == id)
        .ok_or_else(|| "Codex account not found.".to_string())?
        .clone();

    let api = CodexAccountApi::new();
    let home_path = target.codex_home_path.clone();
    let email_hint = target.email_hint.clone();
    let snapshot = tokio::time::timeout(
        std::time::Duration::from_secs(DEFAULT_FETCH_TIMEOUT_SECONDS),
        api.fetch_snapshot(&home_path, email_hint.as_deref(), true),
    )
    .await
    .map_err(|_| "Timed out waiting for the Codex usage API.".to_string())?
    .map_err(into_api_message)?;

    // Persist snapshot to the snapshot store, keyed by account id.
    if let Ok(mut snapshots) = SnapshotStore::new().load() {
        snapshots.insert(target.id, snapshot.clone());
        let _ = SnapshotStore::new().save(&snapshots);
    }

    if refresh_persisted_accounts(app).is_err() {
        // Non-fatal: the snapshot was still fetched.
    }
    Ok(snapshot)
}

#[tauri::command]
pub fn codex_account_snapshots()
-> Result<HashMap<Uuid, codexbar::codex_accounts::AccountUsageSnapshot>, String> {
    SnapshotStore::new().load().map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn codex_account_restart_desktop(
    _app: tauri::AppHandle,
    switch_id: String,
) -> Result<(), String> {
    let _mutation = ACCOUNT_MUTATION
        .try_lock()
        .map_err(|_| "A Codex account operation is already in progress.".to_string())?;
    let pending = PENDING_RESTART.lock().map_err(|e| e.to_string())?.clone();
    let active = CodexAccountManager::new().discover_ambient_account(&[]);
    let pending = validate_pending_restart(pending.as_ref(), &switch_id, active.as_ref())?.clone();
    tauri::async_runtime::spawn_blocking(move || {
        restart_codex_desktop(
            0.8,
            None,
            pending.desktop_session_backup_path.as_deref(),
            pending.desktop_session_restore_path.as_deref(),
        )
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    // The outgoing session backup is single-use. Replaying it after relaunch
    // would overwrite that backup with the newly active account's session.
    *PENDING_RESTART.lock().map_err(|e| e.to_string())? = None;
    Ok(())
}

fn validate_pending_restart<'a>(
    pending: Option<&'a CodexSwitchResult>,
    switch_id: &str,
    active: Option<&CodexAccount>,
) -> Result<&'a CodexSwitchResult, String> {
    pending.filter(|result| {
        result.switch_id.to_string() == switch_id
            && result.ambient_account.as_ref().zip(active).is_some_and(|(expected, current)| expected.matches(current))
    }).ok_or_else(|| "The selected account changed after this restart prompt opened. Switch to the intended account again before restarting Codex Desktop.".into())
}

/// Merge discovered accounts back into the persisted list after identity
/// changes (login/switch) so the store reflects reality.
fn refresh_persisted_accounts(app: tauri::AppHandle) -> Result<(), String> {
    let accounts = load_codex_accounts()?;
    persist_codex_accounts(&accounts)?;
    events::emit_settings_changed(&app);
    accounts_changed(&app);
    Ok(())
}

fn accounts_changed(app: &tauri::AppHandle) {
    events::emit_codex_accounts_updated(app);
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || crate::tray_bridge::rebuild_tray_menu(&handle));
}

fn into_user_message(error: CodexAccountManagerError) -> String {
    match error {
        CodexAccountManagerError::Message(msg) => msg,
        CodexAccountManagerError::Io(e) => e.to_string(),
    }
}

fn into_api_message(error: CodexApiError) -> String {
    match error {
        CodexApiError::Message(msg) => msg,
        CodexApiError::Network(e) => format!("network error: {e}"),
        CodexApiError::Parse(e) => format!("failed to parse Codex payload: {e}"),
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexAccountsStateBridge {
    pub accounts: Vec<CodexAccount>,
    pub snapshots: HashMap<Uuid, codexbar::codex_accounts::AccountUsageSnapshot>,
}

#[tauri::command]
pub fn get_codex_accounts_state(
    state: tauri::State<'_, Mutex<AppState>>,
) -> Result<CodexAccountsStateBridge, String> {
    let _guard = state.lock().map_err(|e| e.to_string())?;
    Ok(CodexAccountsStateBridge {
        accounts: load_codex_accounts()?,
        snapshots: codex_account_snapshots()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_rejects_superseded_prompts_and_external_identity_changes() {
        let active = sample_account();
        let pending = CodexSwitchResult {
            switch_id: Uuid::new_v4(),
            ambient_account: Some(active.clone()),
            materialized_account: None,
            backup_path: None,
            desktop_session_backup_path: None,
            desktop_session_restore_path: None,
            desktop_session_restore_exists: false,
        };
        let id = pending.switch_id.to_string();
        assert!(validate_pending_restart(Some(&pending), &id, Some(&active)).is_ok());
        assert!(
            validate_pending_restart(Some(&pending), &Uuid::new_v4().to_string(), Some(&active))
                .is_err()
        );
        let mut other = active;
        other.provider_account_id = Some("different-account".into());
        assert!(validate_pending_restart(Some(&pending), &id, Some(&other)).is_err());
        assert!(validate_pending_restart(None, &id, Some(&other)).is_err());
    }

    #[test]
    fn codex_switch_supersedes_inflight_usage_and_keeps_other_providers() {
        let mut state = AppState::new();
        let other = invalidate_account_usage(&mut state, ProviderId::Claude);
        let mut old = invalidate_account_usage(&mut state, ProviderId::Codex);
        old.account_email = Some("old@example.com".into());
        old.primary.used_percent = 80.0;
        old.error = None;
        state.provider_cache = vec![other, old];
        state.is_refreshing = true;
        let generation = state.provider_refresh_generation;
        let pending = invalidate_account_usage(&mut state, ProviderId::Codex);
        assert!(!is_current_provider_refresh_generation(&state, generation));
        assert!(!state.is_refreshing);
        assert_eq!(state.provider_cache.len(), 2);
        assert!(
            state
                .provider_cache
                .iter()
                .any(|s| s.provider_id == "claude")
        );
        assert!(pending.account_email.is_none() && pending.error.is_some());
        assert_eq!(pending.primary.used_percent, 0.0);
    }

    #[test]
    fn reconciliation_replaces_changed_managed_identity_without_inheriting_metadata() {
        let mut stale = sample_account();
        stale.nickname = Some("Former account".into());
        let mut fresh = sample_account();
        fresh.provider_account_id = Some("replacement".into());
        fresh.email_hint = Some("replacement@example.com".into());
        let accounts = reconcile_codex_accounts(&[stale.clone()], &[fresh.clone()], None);
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, fresh.id);
        assert_ne!(accounts[0].id, stale.id);
        assert_eq!(accounts[0].nickname, None);
        assert_eq!(accounts[0].email_hint, fresh.email_hint);
    }

    #[test]
    fn reconciliation_preserves_metadata_for_unchanged_managed_identity() {
        let mut stored = sample_account();
        stored.nickname = Some("Work".into());
        let fresh = sample_account();
        let accounts = reconcile_codex_accounts(&[stored.clone()], &[fresh], None);
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, stored.id);
        assert_eq!(accounts[0].nickname, stored.nickname);
    }

    fn sample_account() -> CodexAccount {
        CodexAccount::new(
            Uuid::new_v4(),
            None,
            Some("user@example.com".to_string()),
            Some("auth0|acct".to_string()),
            Some("acct".to_string()),
            std::path::PathBuf::from("/tmp/fake-home"),
            codexbar::codex_accounts::CodexAccountSource::ManagedByApp,
            codexbar::codex_accounts::utc_now(),
            codexbar::codex_accounts::utc_now(),
            Some(codexbar::codex_accounts::utc_now()),
        )
    }

    #[test]
    fn into_user_message_preserves_friendly_text() {
        assert_eq!(
            into_user_message(CodexAccountManagerError::Message(
                "The `codex` command could not be found.".to_string()
            )),
            "The `codex` command could not be found."
        );
    }

    #[test]
    fn sample_account_serializes_camel_case() {
        let json = serde_json::to_value(sample_account()).unwrap();
        assert!(json.get("codexHomePath").is_some());
        assert!(json.get("providerAccountId").is_some());
    }
}
