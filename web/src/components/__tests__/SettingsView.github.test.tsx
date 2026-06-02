// @vitest-environment jsdom
//
// Behavioral coverage for the GitHub settings tab (#1670): the cadence and
// backoff knobs and the two toggles save through the profile-settings path,
// proving the web surface of the 3-surface config parity is wired.

import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsView } from "../SettingsView";
import * as api from "../../lib/api";

const PROFILES = [{ name: "main", is_default: true }];

vi.mock("../../lib/api", () => ({
  fetchProfiles: vi.fn(() => Promise.resolve(PROFILES)),
  fetchSettings: vi.fn(() => Promise.resolve({ github: {} })),
  updateProfileSettings: vi.fn(() => Promise.resolve(true)),
  setCockpitMaster: vi.fn(() => Promise.resolve(true)),
  setDefaultProfile: vi.fn(() => Promise.resolve(true)),
  createProfile: vi.fn(() => Promise.resolve(true)),
  renameProfile: vi.fn(() => Promise.resolve(true)),
  deleteProfile: vi.fn(() => Promise.resolve(true)),
}));

function renderGithubTab() {
  return render(
    <SettingsView
      onClose={() => {}}
      tab="github"
      onSelectTab={vi.fn()}
      serverAbout={null}
      onServerAboutRefresh={() => {}}
    />,
  );
}

function numberInputByLabel(
  container: HTMLElement,
  label: string,
): HTMLInputElement {
  const match = Array.from(container.querySelectorAll("label")).find(
    (l) => l.textContent === label,
  );
  const input = match?.parentElement?.querySelector('input[type="number"]');
  expect(input).toBeTruthy();
  return input as HTMLInputElement;
}

function commit(input: HTMLInputElement, value: string) {
  fireEvent.focus(input);
  fireEvent.change(input, { target: { value } });
  fireEvent.blur(input);
}

function clickToggle(container: HTMLElement, label: string) {
  const labelDiv = Array.from(container.querySelectorAll("div")).find(
    (d) => d.textContent === label && d.querySelector("*") === null,
  );
  const row = labelDiv?.parentElement?.parentElement;
  const sw = row?.querySelector('button[role="switch"]') as HTMLButtonElement;
  expect(sw).toBeTruthy();
  fireEvent.click(sw);
}

describe("Settings GitHub tab", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("saves the cadence and backoff intervals through the profile path", async () => {
    const { container } = renderGithubTab();
    await screen.findByText("GitHub polling enabled");

    commit(numberInputByLabel(container, "Poll interval (s)"), "45");
    commit(numberInputByLabel(container, "Max poll interval (s)"), "600");

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        github: { poll_interval_secs: 45 },
      }),
    );
    expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
      github: { max_poll_interval_secs: 600 },
    });
  });

  it("raises max when the base is set above it", async () => {
    const { container } = renderGithubTab();
    await screen.findByText("GitHub polling enabled");

    // Defaults: base 30, max 300. Setting base to 400 must also bump max.
    commit(numberInputByLabel(container, "Poll interval (s)"), "400");

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        github: { poll_interval_secs: 400 },
      }),
    );
    expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
      github: { max_poll_interval_secs: 400 },
    });
  });

  it("saves the enabled and unauthenticated toggles", async () => {
    const { container } = renderGithubTab();
    await screen.findByText("GitHub polling enabled");

    // enabled defaults to true; clicking flips it to false.
    clickToggle(container, "GitHub polling enabled");
    clickToggle(container, "Allow unauthenticated polling");

    await waitFor(() =>
      expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
        github: { enabled: false },
      }),
    );
    expect(vi.mocked(api.updateProfileSettings)).toHaveBeenCalledWith("main", {
      github: { allow_unauthenticated_polling: true },
    });
  });
});
