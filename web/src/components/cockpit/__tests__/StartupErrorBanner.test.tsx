// @vitest-environment jsdom
//
// Coverage for the cockpit StartupErrorBanner native-binary branch
// and the Open-agent-log disclosure. The disclosure surfaces the
// per-session worker log to dashboard users who don't have host
// terminal access (Tailscale Funnel, remote setups). See #1449.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  render,
  waitFor,
} from "@testing-library/react";

import { ProviderAuthErrorBanner, StartupErrorBanner } from "../CockpitView";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const NATIVE_BINARY_MSG =
  'agent spawn failed: ACP connection failed: Internal error: { "details": "Claude Code native binary at /usr/lib/node_modules/.../claude exists but failed to launch." }';

describe("StartupErrorBanner native-binary branch", () => {
  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        status: 200,
        json: async () => ({
          path: "/tmp/x.log",
          exists: false,
          tail: "",
          lines_returned: 0,
          truncated: false,
        }),
      }),
    );
  });

  it("renders arch/loader remediation copy, not the doctor --fix fallback", () => {
    const { container } = render(
      <StartupErrorBanner sessionId="s-1" agent={null} message={NATIVE_BINARY_MSG} />,
    );
    expect(container.textContent).toContain("Architecture mismatch");
    expect(container.textContent).toContain("dynamic loader");
    expect(container.textContent).toContain("bind-mounted into a container");
    expect(container.textContent).not.toContain("aoe cockpit doctor --fix");
  });

  it("links the native-binary docs anchor", () => {
    const { container } = render(
      <StartupErrorBanner sessionId="s-1" agent={null} message={NATIVE_BINARY_MSG} />,
    );
    const anchor = container.querySelector("a[href*='cockpit']");
    expect(anchor).not.toBeNull();
    expect(anchor?.getAttribute("href")).toContain(
      "native-binary-launch-failure",
    );
  });
});

describe("StartupErrorBanner fallback branch (unchanged)", () => {
  it("still renders the doctor --fix copy on a generic failure", () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        json: async () => ({ exists: false, tail: "" }),
      }),
    );
    const { container } = render(
      <StartupErrorBanner sessionId="s-1" agent={null} message="some unknown failure" />,
    );
    expect(container.textContent).toContain("aoe cockpit doctor --fix");
  });
});

describe("AgentLogDisclosure", () => {
  it("does not fetch until the user clicks Open agent log", () => {
    const fetchSpy = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => ({
        path: "/tmp/x.log",
        exists: true,
        tail: "first line\nsecond line",
        lines_returned: 2,
        truncated: false,
      }),
    });
    vi.stubGlobal("fetch", fetchSpy);
    render(
      <StartupErrorBanner sessionId="s-1" agent={null} message={NATIVE_BINARY_MSG} />,
    );
    expect(fetchSpy).not.toHaveBeenCalled();
  });

  it("fetches the worker-log endpoint on first open and renders the tail", async () => {
    const fetchSpy = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => ({
        path: "/tmp/x.log",
        exists: true,
        tail: "ERROR cockpit.acp: spawn failed\nclaude execve: ENOEXEC",
        lines_returned: 2,
        truncated: false,
      }),
    });
    vi.stubGlobal("fetch", fetchSpy);
    const { getByTestId } = render(
      <StartupErrorBanner sessionId="abc-123" agent={null} message={NATIVE_BINARY_MSG} />,
    );
    fireEvent.click(getByTestId("cockpit-agent-log-toggle"));
    await waitFor(() => {
      expect(fetchSpy).toHaveBeenCalledTimes(1);
    });
    expect(fetchSpy.mock.calls[0]?.[0]).toContain(
      "/api/sessions/abc-123/cockpit/worker-log?tail=200",
    );
    await waitFor(() => {
      expect(getByTestId("cockpit-agent-log-pre").textContent).toContain(
        "ENOEXEC",
      );
    });
  });

  it("renders 'No log output yet' when the endpoint reports exists=false", async () => {
    const fetchSpy = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => ({
        path: "/tmp/x.log",
        exists: false,
        tail: "",
        lines_returned: 0,
        truncated: false,
      }),
    });
    vi.stubGlobal("fetch", fetchSpy);
    const { getByTestId, container } = render(
      <StartupErrorBanner sessionId="s-1" agent={null} message={NATIVE_BINARY_MSG} />,
    );
    fireEvent.click(getByTestId("cockpit-agent-log-toggle"));
    await waitFor(() => {
      expect(container.textContent).toContain("No log output yet");
    });
  });

  it("shows an error message when the fetch fails", async () => {
    const fetchSpy = vi.fn().mockResolvedValue({
      ok: false,
      status: 500,
      text: async () => "boom",
    });
    vi.stubGlobal("fetch", fetchSpy);
    const { getByTestId, container } = render(
      <StartupErrorBanner sessionId="s-1" agent={null} message={NATIVE_BINARY_MSG} />,
    );
    fireEvent.click(getByTestId("cockpit-agent-log-toggle"));
    await waitFor(() => {
      expect(container.textContent).toContain("Could not load log");
      expect(container.textContent).toContain("500");
    });
  });

  it("renders 'log file exists but is empty' when tail is empty and exists=true", async () => {
    const fetchSpy = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => ({
        path: "/tmp/x.log",
        exists: true,
        tail: "",
        lines_returned: 0,
        truncated: false,
      }),
    });
    vi.stubGlobal("fetch", fetchSpy);
    const { getByTestId, container } = render(
      <StartupErrorBanner sessionId="s-1" agent={null} message={NATIVE_BINARY_MSG} />,
    );
    fireEvent.click(getByTestId("cockpit-agent-log-toggle"));
    await waitFor(() => {
      expect(container.textContent).toContain("Log file exists but is empty");
    });
  });

  it("renders the truncated-log hint when the response sets truncated=true", async () => {
    const fetchSpy = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => ({
        path: "/tmp/x.log",
        exists: true,
        tail: "tail content",
        lines_returned: 1,
        truncated: true,
      }),
    });
    vi.stubGlobal("fetch", fetchSpy);
    const { getByTestId, container } = render(
      <StartupErrorBanner sessionId="s-1" agent={null} message={NATIVE_BINARY_MSG} />,
    );
    fireEvent.click(getByTestId("cockpit-agent-log-toggle"));
    await waitFor(() => {
      expect(container.textContent).toContain("Log is large; showing the tail");
    });
  });

  it("hides the body when the toggle is clicked a second time", async () => {
    const fetchSpy = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => ({
        path: "/tmp/x.log",
        exists: true,
        tail: "abc",
        lines_returned: 1,
        truncated: false,
      }),
    });
    vi.stubGlobal("fetch", fetchSpy);
    const { getByTestId, queryByTestId } = render(
      <StartupErrorBanner sessionId="s-1" agent={null} message={NATIVE_BINARY_MSG} />,
    );
    const toggle = getByTestId("cockpit-agent-log-toggle");
    fireEvent.click(toggle);
    await waitFor(() => {
      expect(queryByTestId("cockpit-agent-log-pre")).not.toBeNull();
    });
    fireEvent.click(toggle);
    expect(queryByTestId("cockpit-agent-log-pre")).toBeNull();
  });

  it("re-fetches when the Refresh button is clicked", async () => {
    const fetchSpy = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => ({
        path: "/tmp/x.log",
        exists: true,
        tail: "tail",
        lines_returned: 1,
        truncated: false,
      }),
    });
    vi.stubGlobal("fetch", fetchSpy);
    const { getByTestId } = render(
      <StartupErrorBanner sessionId="s-1" agent={null} message={NATIVE_BINARY_MSG} />,
    );
    fireEvent.click(getByTestId("cockpit-agent-log-toggle"));
    await waitFor(() => {
      expect(fetchSpy).toHaveBeenCalledTimes(1);
    });
    fireEvent.click(getByTestId("cockpit-agent-log-refresh"));
    await waitFor(() => {
      expect(fetchSpy).toHaveBeenCalledTimes(2);
    });
  });
});

// Anti-regression for #1712: provider-auth remediation must be keyed off
// the active agent. A Gemini session must never see Claude-specific
// ANTHROPIC_API_KEY / `claude /login` guidance, and vice versa.
describe("ProviderAuthErrorBanner remediation is provider-aware", () => {
  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: true,
        status: 200,
        json: async () => ({ exists: false, tail: "" }),
      }),
    );
  });

  const GEMINI_INFO = {
    status: "API key expired. Please renew the API key.",
    reason: "API_KEY_INVALID",
  };

  it("renders Gemini-specific copy, never the Claude env var or login", () => {
    const { container, getByTestId } = render(
      <ProviderAuthErrorBanner sessionId="s-1" agent="gemini" info={GEMINI_INFO} />,
    );
    expect(getByTestId("cockpit-provider-auth-banner-s-1")).not.toBeNull();
    expect(container.textContent).toContain("Gemini API key");
    expect(container.textContent).toContain("GEMINI_API_KEY");
    // The raw provider message is surfaced verbatim.
    expect(container.textContent).toContain("renew the API key");
    expect(container.textContent).not.toContain("ANTHROPIC_API_KEY");
    expect(container.textContent).not.toContain("claude /login");
  });

  it("renders Claude-specific copy when the agent is claude", () => {
    const { container } = render(
      <ProviderAuthErrorBanner
        sessionId="s-1"
        agent="claude"
        info={{ status: "invalid x-api-key", reason: null }}
      />,
    );
    expect(container.textContent).toContain("ANTHROPIC_API_KEY");
    expect(container.textContent).toContain("claude /login");
    expect(container.textContent).not.toContain("GEMINI_API_KEY");
  });

  it("renders generic copy for an unknown agent", () => {
    const { container } = render(
      <ProviderAuthErrorBanner
        sessionId="s-1"
        agent={null}
        info={{ status: "auth failed", reason: null }}
      />,
    );
    expect(container.textContent).toContain("configured provider API key");
    expect(container.textContent).not.toContain("GEMINI_API_KEY");
    expect(container.textContent).not.toContain("ANTHROPIC_API_KEY");
  });

  it("hides the banner when dismissed", () => {
    const { queryByTestId, getByText } = render(
      <ProviderAuthErrorBanner sessionId="s-1" agent="gemini" info={GEMINI_INFO} />,
    );
    fireEvent.click(getByText("Dismiss"));
    expect(queryByTestId("cockpit-provider-auth-banner-s-1")).toBeNull();
  });

  it("StartupErrorBanner auth branch is also provider-aware (Gemini)", () => {
    const { container } = render(
      <StartupErrorBanner
        sessionId="s-1"
        agent="gemini"
        message="ACP connection failed: invalid api key"
      />,
    );
    expect(container.textContent).toContain("GEMINI_API_KEY");
    expect(container.textContent).not.toContain("ANTHROPIC_API_KEY");
  });
});
