/**
 * reman for pi: records every bash command pi runs (as agent:pi), with its folder, exit code and
 * duration. reman-hook reads what the output says about the result; the output itself goes
 * nowhere else.
 *
 * Installed by `reman connect pi` (then restart pi or /reload), removed by `reman disconnect pi`.
 */
import type { ExtensionAPI, ExtensionContext } from "@mariozechner/pi-coding-agent"
import { spawn } from "node:child_process"

const HOOK = "@@REMAN_HOOK@@"

function send(run: Record<string, unknown>) {
  try {
    const p = spawn(HOOK, ["event"], { stdio: ["pipe", "ignore", "ignore"], windowsHide: true })
    p.on("error", () => {})
    p.stdin?.on("error", () => {})
    p.stdin?.end(JSON.stringify(run))
  } catch {
    // never get in pi's way
  }
}

function textOf(result: unknown): string {
  const content = (result as { content?: unknown } | undefined)?.content
  if (!Array.isArray(content)) return ""
  return content
    .map((part) => {
      const t = (part as { text?: unknown } | undefined)?.text
      return typeof t === "string" ? t : ""
    })
    .join("\n")
}

// pi's bash tool reports a failure by appending a status line to its text, not a number.
function exitOf(text: string, isError: boolean): number {
  if (!isError) return 0
  const exited = text.match(/Command exited with code (\d+)\s*$/)
  if (exited) return Number(exited[1])
  if (/Command aborted\s*$/.test(text)) return 130
  if (/Command timed out after \S+ seconds\s*$/.test(text)) return 124
  return 1
}

export default function remanPiExtension(pi: ExtensionAPI) {
  // in-flight bash calls, by tool call id
  const pending = new Map<string, { command: string; cwd: string; started: number }>()

  // events, not a bash tool of our own: they fire whichever extension's bash tool runs it
  pi.on("tool_call", async (event, ctx: ExtensionContext) => {
    if (event.toolName !== "bash") return
    const command = (event.input as { command?: unknown }).command
    if (typeof command !== "string" || command.length === 0) return
    pending.set(event.toolCallId, { command, cwd: ctx.cwd, started: Date.now() })
  })

  // tool_execution_end fires even when another extension blocks the call, so nothing is left open
  pi.on("tool_execution_end", async (event, ctx: ExtensionContext) => {
    const p = pending.get(event.toolCallId)
    if (!p) return
    pending.delete(event.toolCallId)
    const text = textOf(event.result)
    send({
      agent: "pi",
      command: p.command,
      cwd: p.cwd || ctx.cwd,
      exit: exitOf(text, event.isError),
      output: text,
      duration_ms: Date.now() - p.started,
    })
  })
}
