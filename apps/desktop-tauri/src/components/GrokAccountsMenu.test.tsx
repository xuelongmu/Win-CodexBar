import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { GrokAccount } from "../types/bridge";

const mocks = vi.hoisted(() => ({
  grokAccountsList: vi.fn(),
  grokAccountSwitch: vi.fn(),
  grokAccountAdd: vi.fn(),
  grokAccountReauthenticate: vi.fn(),
  grokAccountCancelLogin: vi.fn(),
  grokAccountFetch: vi.fn(),
}));
vi.mock("../lib/tauri", () => mocks);
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(() => Promise.resolve(() => {})) }));
vi.mock("../hooks/useLocale", () => ({ useLocale: () => ({ t: (key: string) => key }) }));
import GrokAccountsMenu from "./GrokAccountsMenu";

const first: GrokAccount = {
  id: "user-one",
  email: "one@example.com",
  organization: null,
  plan: "SuperGrok",
  isActive: true,
  isSaved: true,
};
const second: GrokAccount = {
  ...first,
  id: "user-two",
  email: "two@example.com",
  isActive: false,
};

describe("GrokAccountsMenu", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    mocks.grokAccountsList.mockResolvedValue([first, second]);
    mocks.grokAccountFetch.mockImplementation(async (id: string) => ({
      usageAvailable: true,
      usedPercent: id === first.id ? 38 : 100,
      plan: "SuperGrok",
      windowMinutes: 10080,
      resetsAt: "2026-09-25T05:55:45Z",
    }));
  });

  it("shows Codex-style usage bars for each Grok account", async () => {
    const { container } = render(
      <GrokAccountsMenu hideEmail={false} resetTimeRelative />,
    );
    await screen.findByText(first.email);
    expect(screen.getAllByText("7d").length).toBeGreaterThan(0);
    expect(screen.getByText("38% PanelUsedSuffix")).toBeInTheDocument();
    expect(screen.getByText("100% PanelUsedSuffix")).toBeInTheDocument();
    const bars = container.querySelectorAll(".codex-menu-accounts__bar-fill");
    expect(bars).toHaveLength(2);
    expect((bars[0] as HTMLElement).style.width).toBe("38%");
    expect((bars[1] as HTMLElement).style.width).toBe("100%");
    expect(screen.queryByRole("button", { name: /CodexAccountsReauthenticateButton/ })).toBeNull();
  });

  it("offers targeted login only for an authentication failure and hides it after recovery", async () => {
    mocks.grokAccountFetch.mockImplementation(async (id: string) => ({ usageAvailable: false, usedPercent: null, plan: null, windowMinutes: null, resetsAt: null, needsAuthentication: id === second.id, usageError: "Usage unavailable." }));
    mocks.grokAccountReauthenticate.mockResolvedValue(undefined);
    render(<GrokAccountsMenu hideEmail={false} resetTimeRelative />);
    const refresh = await screen.findByRole("button", { name: `CodexAccountsReauthenticateButton: ${second.email}` });
    expect(screen.getAllByRole("button", { name: /CodexAccountsReauthenticateButton/ })).toHaveLength(1);
    mocks.grokAccountFetch.mockResolvedValue({ usageAvailable: true, usedPercent: 42, plan: null, windowMinutes: null, resetsAt: null, needsAuthentication: false });
    await act(async () => fireEvent.click(refresh));
    expect(mocks.grokAccountReauthenticate).toHaveBeenCalledWith(second.id);
    expect(mocks.grokAccountSwitch).not.toHaveBeenCalled();
    expect(mocks.grokAccountAdd).not.toHaveBeenCalled();
    expect(screen.queryByRole("button", { name: /CodexAccountsReauthenticateButton/ })).toBeNull();
  });

  it("omits percentage and bar when usage is unavailable", async () => {
    mocks.grokAccountFetch.mockResolvedValue({
      usageAvailable: false,
      usedPercent: null,
      plan: "SuperGrok",
      windowMinutes: 10080,
      resetsAt: null,
    });
    const { container } = render(
      <GrokAccountsMenu hideEmail={false} resetTimeRelative />,
    );

    await screen.findByText(first.email);
    expect(screen.queryByText(/PanelUsedSuffix/)).toBeNull();
    expect(container.querySelectorAll(".codex-menu-accounts__bar-fill")).toHaveLength(0);
  });

  it("shows a single active account and adds from the footer with cancellable sign-in", async () => {
    mocks.grokAccountsList.mockResolvedValue([first]);
    let finishLogin: (() => void) | undefined;
    mocks.grokAccountAdd.mockImplementation(() => new Promise<void>(resolve => { finishLogin = resolve; }));
    mocks.grokAccountCancelLogin.mockResolvedValue(undefined);
    const { container } = render(<GrokAccountsMenu hideEmail={false} resetTimeRelative />);
    const account = await screen.findByText(first.email);
    expect(container.querySelector("details")?.open).toBe(true);
    const add = screen.getByRole("button", { name: "CodexAccountsAddButton" });
    expect(account.compareDocumentPosition(add) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    await act(async () => fireEvent.click(add));
    expect(mocks.grokAccountAdd).toHaveBeenCalledOnce();
    expect(add).toBeDisabled();
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "GrokAccountsCancelLogin" })));
    expect(mocks.grokAccountCancelLogin).toHaveBeenCalledOnce();
    mocks.grokAccountsList.mockResolvedValue([first, second]);
    await act(async () => finishLogin?.());
    expect(await screen.findByText(second.email)).toBeInTheDocument();
    expect(add).toBeEnabled();
    expect(mocks.grokAccountSwitch).not.toHaveBeenCalled();
  });

  it("keeps the add action available with no saved accounts and reports login failures", async () => {
    mocks.grokAccountsList.mockResolvedValue([]);
    mocks.grokAccountAdd.mockRejectedValue("Login cancelled.");
    render(<GrokAccountsMenu hideEmail={false} resetTimeRelative />);
    const add = screen.getByRole("button", { name: "CodexAccountsAddButton" });
    await waitFor(() => expect(add).toBeEnabled());
    await act(async () => fireEvent.click(add));
    expect(screen.getByRole("alert")).toHaveTextContent("Login cancelled.");
    expect(add).toBeEnabled();
  });
});
