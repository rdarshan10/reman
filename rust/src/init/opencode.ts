/**
 * reman for opencode: records every bash command opencode runs (as agent:opencode), with its
 * folder, exit code and duration. reman-hook reads what the output says about the result (a
 * piped `npx jest | tail` still says `Tests: 2 failed`); the output itself goes nowhere else.
 *
 * Installed by `reman connect opencode`, removed by `reman disconnect opencode`.
 */
import type { Plugin } from "@opencode-ai/plugin"
import { spawn } from "node:child_process"

const HOOK = "@@REMAN_HOOK@@"

type Pending = { command: string; cwd: string; started: number }

function send(run: Record<string, unknown>) {
  try {
    const p = spawn(HOOK, ["event"], { stdio: ["pipe", "ignore", "ignore"], windowsHide: true })
    p.on("error", () => {})
    p.stdin?.on("error", () => {})
    p.stdin?.end(JSON.stringify(run))
  } catch {
    // never get in opencode's way
  }
}

// The bash tool's exit code, else what its text says; unknown rather than a guess.
function exitOf(output: { output?: unknown; metadata?: Record<string, unknown> }): number | null {
  const exit = output.metadata?.exit
  if (typeof exit === "number") return exit
  const text = typeof output.output === "string" ? output.output : ""
  if (text.includes("User aborted the command")) return 130
  if (/exceeding timeout \d+ ms/.test(text)) return 124
  return null
}

export const RemanPlugin: Plugin = async ({ directory }) => {
  // in-flight bash calls, by tool call id
  const pending = new Map<string, Pending>()
  return {
    "tool.execute.before": async (input, output) => {
      if (input.tool !== "bash") return
      const command = (output.args as { command?: unknown } | undefined)?.command
      if (typeof command !== "string" || !command.trim()) return
      pending.set(input.callID, { command, cwd: directory, started: Date.now() })
    },
    // where the shell really runs (a `workdir` argument moves it)
    "shell.env": async (input: unknown) => {
      const i = input as { callID?: string; cwd?: unknown }
      const p = pending.get(i.callID ?? "")
      if (p && typeof i.cwd === "string" && i.cwd) p.cwd = i.cwd
    },
    "tool.execute.after": async (input, output) => {
      const p = pending.get(input.callID)
      if (!p) return
      pending.delete(input.callID)
      const o = output as { output?: unknown; metadata?: Record<string, unknown> }
      send({
        agent: "opencode",
        command: p.command,
        cwd: p.cwd,
        session: input.sessionID,
        exit: exitOf(o),
        output: typeof o.output === "string" ? o.output : "",
        duration_ms: Date.now() - p.started,
      })
    },
  }
}
