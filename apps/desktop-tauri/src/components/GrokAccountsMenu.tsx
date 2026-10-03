import { useEffect, useState } from "react";
import type { GrokAccount, GrokAccountUsage } from "../types/bridge";
import { grokAccountAdd, grokAccountReauthenticate, grokAccountCancelLogin, grokAccountSwitch } from "../lib/tauri";
import { useLocale } from "../hooks/useLocale";
import { useFormattedResetTime } from "../hooks/useFormattedResetTime";
import { useGrokAccounts } from "../hooks/useGrokAccounts";
import { maskEmail } from "./MenuCard";
import ProviderAccountsMenu from "./ProviderAccountsMenu";

export default function GrokAccountsMenu({
  hideEmail,
  resetTimeRelative,
  onLayoutChange,
  }: {
  hideEmail: boolean;
  resetTimeRelative: boolean;
  onLayoutChange?: () => void;
}) {
  const { t } = useLocale();
  const [switched, setSwitched] = useState(false);
  const [signingIn, setSigningIn] = useState(false);
  const { accounts, usage, busy, error, reportError, run } = useGrokAccounts({ reloadOnFocus: true });
  useEffect(() => {
    onLayoutChange?.();
  }, [accounts, error, switched, signingIn, onLayoutChange]);

  const switchAccount = async (id: string) => {
    setSwitched(false);
    await run(() => grokAccountSwitch(id), () => setSwitched(true));
  };

  const addAccount = (id?: string) => {
    setSwitched(false);
    setSigningIn(true);
    void run(id ? () => grokAccountReauthenticate(id) : grokAccountAdd, undefined, () => setSigningIn(false));
  };
  return (
    <ProviderAccountsMenu
      title={t("GrokAccountsTitle")}
      count={accounts.length}
      aria-busy={busy}
      onLayoutChange={onLayoutChange}
      actions={<>
        <button type="button" className="codex-menu-accounts__action" disabled={busy} onClick={() => addAccount()}>
          {t("CodexAccountsAddButton")}
        </button>
        {signingIn && <button type="button" className="codex-menu-accounts__action" onClick={() => void grokAccountCancelLogin().catch(reportError)}>
          {t("GrokAccountsCancelLogin")}
        </button>}
      </>}
    >
      {error && (
        <div className="codex-menu-accounts__error" role="alert">
          {error}
        </div>
      )}
      {switched && <p role="status">{t("GrokAccountsSwitched")}</p>}
      {signingIn && <p className="codex-menu-accounts__usage" role="status">{t("GrokAccountsSigningIn")}</p>}
      <ul className="codex-menu-accounts__list">
        {accounts.map((account) => (
          <GrokAccountRow
            key={account.id}
            account={account}
            snapshot={usage[account.id]}
            hideEmail={hideEmail}
            resetTimeRelative={resetTimeRelative}
            busy={busy}
            onSwitch={switchAccount}
            onReauthenticate={addAccount}
          />
        ))}
      </ul>
    </ProviderAccountsMenu>
  );
}

function GrokAccountRow({
  account,
  snapshot,
  hideEmail,
  resetTimeRelative,
  busy,
  onSwitch,
  onReauthenticate,
}: {
  account: GrokAccount;
  snapshot: GrokAccountUsage | undefined;
  hideEmail: boolean;
  resetTimeRelative: boolean;
  busy: boolean;
  onSwitch: (id: string) => Promise<void>;
  onReauthenticate: (id: string) => void;
}) {
  const { t } = useLocale();
  const email = hideEmail ? maskEmail(account.email) : account.email;
  const pct =
    snapshot?.usageAvailable && snapshot.usedPercent != null
      ? Math.round(snapshot.usedPercent)
      : null;
  const resetText = useFormattedResetTime(
    snapshot?.resetsAt ?? null,
    null,
    resetTimeRelative,
  );
  const resetLabel = resetText
    ? resetTimeRelative
      ? resetText
      : `${t("MetricResetsIn")} ${resetText}`
    : null;
  const windowLabel = formatWindowLabel(snapshot?.windowMinutes);
  const barLevel =
    pct != null && pct >= 100 ? "exhausted" : pct != null && pct >= 90 ? "critical" : undefined;

  return (
    <li>
      <div
        className={`codex-menu-accounts__row${account.isActive ? " codex-menu-accounts__row--active" : ""}`}
      >
        <div className="codex-menu-accounts__meta">
          <span className="codex-menu-accounts__email" title={email}>
            {email}
            {account.isActive && (
              <span className="codex-menu-accounts__badge">
                {t("TokenAccountActive")}
              </span>
            )}
          </span>
          {(pct !== null || resetLabel) && (
            <span className="codex-menu-accounts__usage">
              {windowLabel && <span>{windowLabel}</span>}
              {pct !== null && <span>{pct}% {t("PanelUsedSuffix")}</span>}
              {resetLabel && <span>{resetLabel}</span>}
            </span>
          )}
          {pct !== null && (
            <span className="codex-menu-accounts__bar" aria-hidden>
              <span
                className="codex-menu-accounts__bar-fill"
                data-level={barLevel}
                style={{ width: `${Math.max(2, Math.min(100, pct))}%` }}
              />
            </span>
          )}
          {snapshot?.usageError && <span className="codex-menu-accounts__error" role="status">{snapshot.usageError}</span>}
        </div>
        <div className="codex-menu-accounts__row-actions">
          {snapshot?.needsAuthentication && (
            <button
              type="button"
              className="codex-menu-accounts__switch"
              disabled={busy}
              aria-label={`${t("CodexAccountsReauthenticateButton")}: ${email}`}
              onClick={() => onReauthenticate(account.id)}
            >
              {t("CodexAccountsReauthenticateButton")}
            </button>
          )}
          <button
            type="button"
            className="codex-menu-accounts__switch"
            disabled={busy || account.isActive || !account.isSaved}
            onClick={() => void onSwitch(account.id)}
          >
            {t("CodexAccountsSwitchButton")}
          </button>
        </div>
      </div>
    </li>
  );
}

function formatWindowLabel(windowMinutes: number | null | undefined): string | null {
  if (!windowMinutes || windowMinutes <= 0) return null;
  if (windowMinutes % 1_440 === 0) return `${windowMinutes / 1_440}d`;
  if (windowMinutes % 60 === 0) return `${windowMinutes / 60}h`;
  return null;
}
