// @vitest-environment jsdom
/**
 * The Processes panel's resource view, against every shape the backend can send.
 *
 * The eight cases below are eight *different answers*, not eight renderings of
 * one. That is the whole point of the view: before it, a user could not tell a
 * kernel-held bound from a supervised one, an unenforced field from an unset one,
 * or a resource kill from a crash — and every one of those distinctions is
 * load-bearing. A kernel bound survives this app dying and a supervised one does
 * not; a limit nothing holds is a promise the app is not keeping.
 *
 * Two properties are asserted throughout, because they are the ways a resource UI
 * lies:
 *
 * 1. **Nothing unsupported renders as zero.** A budget of nothing and a budget
 *    nobody enforces read identically as "0", and one of them is a false claim of
 *    safety.
 * 2. **No mechanism text is invented here.** Every mechanism, level and reason
 *    asserted below is the string the backend sent, so a test passing means the
 *    panel is showing the backend's own answer rather than a table this file
 *    maintains in parallel.
 */
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

import { ProcessResources } from "./ProcessResources";
import type {
  LimitBreach,
  ProcessLimitReport,
  ProcessResourceReport,
} from "../../lib/processTable";

afterEach(cleanup);

function limit(overrides: Partial<ProcessLimitReport> = {}): ProcessLimitReport {
  return {
    limit: "max_memory_bytes",
    classDefault: 8 * 1024 * 1024 * 1024,
    effective: 8 * 1024 * 1024 * 1024,
    origin: "class_default",
    supportStatus: "enforced",
    supportDetail: "the resource controller bounds the owned process tree",
    ...overrides,
  };
}

function report(overrides: Partial<ProcessResourceReport> = {}): ProcessResourceReport {
  return {
    processId: "fgsh-1",
    kind: "foreground_shell",
    backend: "supervisor",
    treePrimitive: "POSIX process group, unioned with the parent-link closure",
    backendIsRecorded: true,
    limits: [limit()],
    ...overrides,
  };
}

describe("the resource view", () => {
  it("shows a bounded process's effective number, its source and what holds it", () => {
    render(<ProcessResources report={report()} />);

    expect(screen.getByText("8 GiB")).toBeTruthy();
    expect(screen.getByText("Memory")).toBeTruthy();
    expect(screen.getByText("The default for this kind.")).toBeTruthy();
  });

  it("names a kernel-held bound as kernel, with the kernel's own mechanism", () => {
    render(
      <ProcessResources
        report={report({
          backend: "cgroup v2",
          limits: [
            limit({
              host: {
                status: "enforced",
                level: "kernel",
                mechanism: "cgroup v2 `memory.max`, with `memory.swap.max` at zero",
              },
            }),
          ],
        })}
      />,
    );

    expect(screen.getByText("Kernel")).toBeTruthy();
    expect(
      screen.getByText("cgroup v2 `memory.max`, with `memory.swap.max` at zero"),
    ).toBeTruthy();
    expect(screen.getByText("Enforced here by: cgroup v2")).toBeTruthy();
  });

  it("names a supervised bound as supervised rather than borrowing the word kernel", () => {
    render(
      <ProcessResources
        report={report({
          limits: [
            limit({
              host: {
                status: "enforced",
                level: "supervised",
                mechanism: "summed resident size over the tree",
              },
            }),
          ],
        })}
      />,
    );

    expect(screen.getByText("Supervised")).toBeTruthy();
    expect(screen.queryByText("Kernel")).toBeNull();
  });

  it("shows an owner-sourced bound as such, so its number is not read as this row's", () => {
    render(
      <ProcessResources
        report={report({
          kind: "daemon_job",
          backend: undefined,
          treePrimitive: undefined,
          limits: [
            limit({
              origin: "unbounded",
              classDefault: null,
              effective: null,
              supportStatus: "owner-sourced",
              supportDetail: "the daemon's watchdog measures against the recipe's own budget",
            }),
          ],
        })}
      />,
    );

    expect(screen.getByText("Owner-sourced")).toBeTruthy();
    expect(screen.getByText("no limit")).toBeTruthy();
  });

  it("renders an unavailable resource as its reason, never as zero", () => {
    render(
      <ProcessResources
        report={report({
          limits: [
            limit({
              limit: "max_context_tokens",
              classDefault: null,
              effective: null,
              origin: "unbounded",
              supportStatus: "unavailable",
              supportDetail: "this kind issues no model request of its own",
            }),
          ],
        })}
      />,
    );

    expect(screen.getByText("Unavailable")).toBeTruthy();
    expect(screen.getByText("this kind issues no model request of its own")).toBeTruthy();
    // The property this whole view turns on.
    expect(screen.queryByText("0")).toBeNull();
    expect(screen.queryByText("0 B")).toBeNull();
  });

  it("distinguishes a not-applicable resource from a missing mechanism", () => {
    render(
      <ProcessResources
        report={report({
          limits: [
            limit({
              limit: "max_context_tokens",
              classDefault: null,
              effective: null,
              origin: "unbounded",
              supportStatus: "enforced",
              supportDetail: "checked before the request",
              host: {
                status: "not_applicable",
                reason: "a resource controller bounds an OS process, not a context budget",
              },
            }),
          ],
        })}
      />,
    );

    expect(screen.getByText("Not applicable")).toBeTruthy();
    expect(screen.queryByText("Unavailable")).toBeNull();
  });

  it("shows a caller's tightening against the default it tightened", () => {
    render(
      <ProcessResources
        report={report({
          limits: [limit({ effective: 512 * 1024 * 1024, origin: "caller_override" })],
        })}
      />,
    );

    expect(screen.getByText("512 MiB")).toBeTruthy();
    expect(screen.getByText("Tightened by the caller, below the 8 GiB default.")).toBeTruthy();
  });

  it("reports an unmeasured resource with its reason instead of a number", () => {
    render(
      <ProcessResources
        report={report({
          limits: [
            limit({
              observedUnavailable: "nothing sampled this process's resource use while it ran",
            }),
          ],
        })}
      />,
    );

    expect(
      screen.getByText("Not measured: nothing sampled this process's resource use while it ran"),
    ).toBeTruthy();
  });

  it("shows what a process is holding now beside the highest it ever held", () => {
    render(
      <ProcessResources
        report={report({
          limits: [
            limit({ observed: 3 * 1024 * 1024, observedPeak: 7 * 1024 * 1024 * 1024 }),
          ],
        })}
      />,
    );

    // Both numbers, because only the second one says whether an 8 GiB ceiling
    // was nearly hit by a build that is now idle.
    expect(screen.getByText("Now 3 MiB · peak 7 GiB")).toBeTruthy();
  });

  it("marks a half-measured resource rather than printing a zero for the other half", () => {
    render(
      <ProcessResources report={report({ limits: [limit({ observed: 3 * 1024 * 1024 })] })} />,
    );

    expect(screen.getByText("Now 3 MiB · peak not measured")).toBeTruthy();
  });

  it("says so when a process recorded no enforcement mechanism at all", () => {
    // The distinction the panel must not blur: a row that cannot name what held
    // it is not the same as one that can, and rendering nothing made them look
    // identical.
    render(
      <ProcessResources
        report={report({ backend: undefined, backendIsRecorded: false })}
      />,
    );

    expect(
      screen.getByText(
        "This process recorded no enforcement mechanism, so what held it cannot be stated.",
      ),
    ).toBeTruthy();
  });
});

describe("a breach", () => {
  function breach(overrides: Partial<LimitBreach> = {}): LimitBreach {
    return {
      limit: "max_memory_bytes",
      configured: 512 * 1024 * 1024,
      observed: 620 * 1024 * 1024,
      backend: "supervisor",
      level: "supervised",
      observedAtMs: 1_800_000_000_000,
      ...overrides,
    };
  }

  it("shows a supervised breach as the two numbers that found it", () => {
    render(<ProcessResources report={report({ breach: breach() })} />);

    expect(screen.getByText("Memory limit exceeded")).toBeTruthy();
    expect(screen.getByText("Configured: 512 MiB")).toBeTruthy();
    expect(screen.getByText("Observed: 620 MiB")).toBeTruthy();
    expect(screen.getByText("Enforcement: supervised (supervisor)")).toBeTruthy();
  });

  it("explains a kernel breach whose two numbers are equal", () => {
    render(
      <ProcessResources
        report={report({
          breach: breach({
            limit: "max_child_processes",
            configured: 12,
            observed: 12,
            backend: "cgroup v2",
            level: "kernel",
            evidence: "cgroup v2 `pids.events` max, the kernel refusing a fork at the cap",
          }),
        })}
      />,
    );

    expect(screen.getByText("Child processes limit exceeded")).toBeTruthy();
    expect(screen.getByText("Configured: 12")).toBeTruthy();
    expect(screen.getByText("Observed: 12")).toBeTruthy();
    expect(
      screen.getByText(
        "Evidence: cgroup v2 `pids.events` max, the kernel refusing a fork at the cap",
      ),
    ).toBeTruthy();
    // Without this, the limit that worked best reads as the one that did not fire.
    expect(
      screen.getByText(
        "The two numbers are equal because the kernel refused the work rather than letting the measurement pass the cap.",
      ),
    ).toBeTruthy();
  });

  it("shows no breach panel for a process that ended on its own terms", () => {
    render(<ProcessResources report={report()} />);

    expect(screen.queryByText(/limit exceeded/)).toBeNull();
  });

  it("renders a browser session's split ownership on one row", () => {
    render(
      <ProcessResources
        report={report({
          kind: "browser_session",
          backend: "supervisor",
          limits: [
            limit({
              limit: "max_wall_ms",
              classDefault: 600_000,
              effective: 600_000,
              supportStatus: "owner-sourced",
              supportDetail: "the browser watchdog enforces its session's own `max_session_ms`",
            }),
            limit({
              classDefault: 4 * 1024 * 1024 * 1024,
              effective: 4 * 1024 * 1024 * 1024,
              host: {
                status: "enforced",
                level: "supervised",
                mechanism: "summed resident size over the tree",
              },
            }),
          ],
        })}
      />,
    );

    // The session clock belongs to the browser watchdog; the memory bound to the
    // resource controller. One resource, one owner, and the row says which.
    expect(screen.getByText("Owner-sourced")).toBeTruthy();
    expect(screen.getByText("Supervised")).toBeTruthy();
    expect(screen.getByText("10 min")).toBeTruthy();
    expect(screen.getByText("4 GiB")).toBeTruthy();
  });

  it("says the number is unknown for a legacy row that predates its class default", () => {
    render(
      <ProcessResources report={report({ limits: [limit({ effective: null, origin: "unrecorded" })] })} />,
    );

    expect(screen.getByText("no limit")).toBeTruthy();
    expect(
      screen.getByText(
        "Recorded before this kind had a default, so the number it ran under is unknown.",
      ),
    ).toBeTruthy();
  });
});
