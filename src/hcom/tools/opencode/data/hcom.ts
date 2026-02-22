import type { Plugin, PluginInput } from "@opencode-ai/plugin"
import type { Event } from "@opencode-ai/sdk"
import { appendFileSync } from "fs"
import { homedir } from "os"

const HCOM_DIR = process.env.HCOM_DIR || `${homedir()}/.hcom`
const LOG_PATH = `${HCOM_DIR}/.tmp/logs/hcom.log`

function log(
  level: "DEBUG" | "INFO" | "WARN" | "ERROR",
  event: string,
  instance?: string | null,
  extra?: Record<string, unknown>,
) {
  const entry = JSON.stringify({
    ts: new Date().toISOString().replace(/\.\d{3}Z$/, "Z"),
    level,
    subsystem: "plugin",
    event,
    ...(instance ? { instance } : {}),
    ...extra,
  })
  try { appendFileSync(LOG_PATH, entry + "\n") } catch {}
}

export const HcomPlugin: Plugin = async ({ client, $ }) => {
  let hcomChecked = false
  let hcomAvailable = false
  let instanceName: string | null = null      // IDEN-03: bound instance name
  let sessionId: string | null = null         // IDEN-02: tracked for messages.transform
  let bootstrapText: string | null = null     // BOOT-01: cached from opencode-start
  let bindingPromise: Promise<void> | null = null  // Prevents duplicate binding
  let reconcileTimer: ReturnType<typeof setInterval> | null = null  // Periodic status sync + delivery fallback
  let notifyServer: ReturnType<typeof Bun.listen> | null = null  // TCP notify server for instant message wake
  let lastReportedStatus: string | null = null  // Skip redundant status updates

  // SAFE-02: Lazy PATH detection on first hook callback
  function checkHcom(): boolean {
    if (!hcomChecked) {
      hcomChecked = true
      hcomAvailable = Bun.which("hcom") !== null
      if (!hcomAvailable) {
        log("WARN", "plugin.no_hcom")
      }
    }
    return hcomAvailable
  }

  function findLastUserMessage(
    messages: Array<{ info: { id: string; sessionID: string; role: string }; parts: any[] }>
  ) {
    for (let i = messages.length - 1; i >= 0; i--) {
      if (messages[i].info.role === "user") return messages[i]
    }
    return null
  }

  function formatMessagesForInjection(messages: any[], recipientName: string): string {
    const parts = messages.map((m: any) => {
      const prefix = m.intent
        ? m.thread
          ? `[${m.intent}:${m.thread} #${m.event_id}]`
          : `[${m.intent} #${m.event_id}]`
        : m.thread
          ? `[thread:${m.thread} #${m.event_id}]`
          : `[new message #${m.event_id}]`
      return `${prefix} ${m.from} -> ${recipientName}: ${m.message}`
    })
    if (messages.length === 1) return `<hcom>${parts[0]}</hcom>`
    return `<hcom>[${messages.length} new messages] | ${parts.join(" | ")}</hcom>`
  }

  // Deliver pending messages to idle agent via promptAsync, then ack immediately.
  async function deliverPendingToIdle(sid: string): Promise<boolean> {
    if (!instanceName) return false
    const msgResult = await $.nothrow()`hcom opencode-read --name ${instanceName}`.quiet()
    if (msgResult.exitCode !== 0) return false
    let rawMessages: any[] = []
    try { rawMessages = JSON.parse(msgResult.text()) } catch { return false }
    if (!Array.isArray(rawMessages) || rawMessages.length === 0) return false

    const maxId = Math.max(...rawMessages.map((m: any) => m.event_id || 0))
    if (maxId === 0) return false

    const formatted = formatMessagesForInjection(rawMessages, instanceName)
    // Ack BEFORE promptAsync — cursor must advance before the new turn's
    // messages.transform reads, otherwise it sees the same messages again.
    await $.nothrow()`hcom opencode-read --name ${instanceName} --ack --up-to ${String(maxId)}`.quiet()
    client.session.promptAsync({
      path: { id: sid },
      body: { parts: [{ type: "text", text: formatted }] },
    } as any)
    log("INFO", "plugin.idle_delivery", instanceName, {
      msg: `promptAsync + acked to ${maxId}`,
      count: rawMessages.length,
      acked_to: maxId,
    })
    return true
  }

  // Periodic status sync: polls session status API as a retry mechanism
  // in case the event-driven opencode-status call failed (subprocess error,
  // daemon down, etc. other made up scenario etc.). Does NOT deliver messages — that's handled by
  // TCP notify (on message arrival) and session.status events (on idle).
  async function reconcile(): Promise<void> {
    if (!instanceName || !sessionId) return
    try {
      const statusResult = await client.session.status()
      if (!statusResult.data) return
      const current = statusResult.data[sessionId]
      const isIdle = !current || current.type === "idle"
      const hcomStatus = isIdle ? "listening" : "active"
      if (hcomStatus !== lastReportedStatus) {
        lastReportedStatus = hcomStatus
        await $.nothrow()`hcom opencode-status --name ${instanceName} --status ${hcomStatus}`.quiet()
        log("INFO", "plugin.reconcile_status", instanceName, { status: hcomStatus })
      }
    } catch (e) {
      log("ERROR", "plugin.reconcile_error", instanceName, { error: String(e) })
    }
  }

  function startReconcileTimer(): void {
    stopReconcileTimer()
    reconcileTimer = setInterval(() => { reconcile() }, 5_000)
  }

  function stopReconcileTimer(): void {
    if (reconcileTimer) { clearInterval(reconcileTimer); reconcileTimer = null }
  }

  // TCP notify server: instant wake when hcom messages arrive.
  // notify_all_instances() TCP-connects to this port on every send.
  function startNotifyServer(): number | null {
    if (notifyServer) return notifyServer.port
    try {
      notifyServer = Bun.listen({
        hostname: "127.0.0.1",
        port: 0,
        socket: {
          open(socket) {
            socket.end()
            if (sessionId && instanceName) deliverPendingToIdle(sessionId)
          },
          data() {},
          close() {},
          error() {},
        },
      })
      log("INFO", "notify_server.started", instanceName, { port: notifyServer.port })
      return notifyServer.port
    } catch (e) {
      log("ERROR", "notify_server.start_failed", instanceName, { error: String(e) })
      return null
    }
  }

  function stopNotifyServer(): void {
    if (notifyServer) {
      try { notifyServer.stop(true) } catch {}
      notifyServer = null
    }
  }

  async function bindIdentity(sid: string): Promise<void> {
    if (instanceName || bindingPromise) return
    if (process.env.HCOM_LAUNCHED !== "1") return

    bindingPromise = (async () => {
      try {
        // Start TCP notify server before binding so port is registered atomically
        const notifyPort = startNotifyServer()
        const result = notifyPort
          ? await $.nothrow()`hcom opencode-start --session-id ${sid} --notify-port ${String(notifyPort)}`.quiet()
          : await $.nothrow()`hcom opencode-start --session-id ${sid}`.quiet()
        if (result.exitCode !== 0) { stopNotifyServer(); return }
        const json = JSON.parse(result.text())
        if (json.error) {
          log("WARN", "plugin.bind_failed", null, { error: json.error })
          stopNotifyServer()
          return
        }
        instanceName = json.name
        sessionId = json.session_id
        bootstrapText = json.bootstrap || null
        log("INFO", "plugin.bound", instanceName, { session_id: sessionId, notify_port: notifyPort })
      } catch (e) {
        log("ERROR", "plugin.bind_error", null, { error: String(e) })
        stopNotifyServer()
      } finally {
        bindingPromise = null
      }
    })()
    await bindingPromise
  }

  return {
    event: async ({ event }: { event: Event }) => {
      try {
        if (!checkHcom()) return
        const eventSessionId = event.properties?.sessionID ?? event.properties?.info?.id
        if (eventSessionId && !sessionId) {
          sessionId = eventSessionId as string
        }
        switch (event.type) {
          case "session.created": {
            const createdSessionId = event.properties.info.id
            log("INFO", "plugin.session_created", instanceName, { session_id: createdSessionId })
            if (createdSessionId && !instanceName && !bindingPromise) {
              await bindIdentity(createdSessionId)
            }
            break
          }
          case "session.status": {
            const statusType = event.properties.status.type
            const eventSessionId = event.properties.sessionID

            log("DEBUG", "plugin.session_status", instanceName, { status: statusType })

            // Bind identity on resume (session.created doesn't fire for existing sessions)
            if (eventSessionId && !instanceName && !bindingPromise) {
              await bindIdentity(eventSessionId)
            }

            // Report status to hcom daemon (skip if unchanged)
            if (instanceName) {
              const hcomStatus = statusType === "idle" ? "listening" : "active"
              if (hcomStatus !== lastReportedStatus) {
                lastReportedStatus = hcomStatus
                await $.nothrow()`hcom opencode-status --name ${instanceName} --status ${hcomStatus}`.quiet()
              }
              // Ensure reconcile timer is running (catches missed idle events)
              startReconcileTimer()
            }

            // Idle transition: deliver any pending messages
            if (statusType === "idle" && instanceName && eventSessionId) {
              await deliverPendingToIdle(eventSessionId)
            }
            break
          }
          case "session.deleted":
            log("INFO", "plugin.session_deleted", instanceName)
            stopNotifyServer()
            stopReconcileTimer()
            if (instanceName) {
              await $.nothrow()`hcom opencode-stop --name ${instanceName} --reason closed`.quiet()
            }
            instanceName = null
            sessionId = null
            bootstrapText = null
            bindingPromise = null
            lastReportedStatus = null
            break
          case "file.edited": {
            const filePath = event.properties.file
            if (instanceName) {
              await $.nothrow()`hcom opencode-status --name ${instanceName} --status active --context ${"tool:write"} --detail ${filePath}`.quiet()
            }
            break
          }
        }
      } catch (e) {
        log("ERROR", "plugin.event_error", instanceName, { error: String(e) })
      }
    },

    "chat.message": async (input, output) => {
      try {
        if (!checkHcom()) return
        if (input.sessionID && !sessionId) {
          sessionId = input.sessionID
        }
        if (bindingPromise) await bindingPromise
        if (input.sessionID && !instanceName) {
          await bindIdentity(input.sessionID)
        }
        log("DEBUG", "plugin.chat_message", instanceName, {
          session_id: input.sessionID,
          agent: input.agent,
          model: input.model?.modelID,
        })
      } catch (e) {
        log("ERROR", "plugin.chat_message_error", instanceName, { error: String(e) })
      }
    },

    "experimental.chat.messages.transform": async (input, output) => {
      try {
        if (!checkHcom()) return
        if (bindingPromise) await bindingPromise
        if (!instanceName && sessionId) await bindIdentity(sessionId)
        if (!instanceName || !sessionId) return

        // Inject bootstrap on first user message (ephemeral — clone discarded after each turn)
        if (bootstrapText) {
          const firstUserMsg = output.messages.find((m: any) => m.info.role === "user")
          if (firstUserMsg) {
            firstUserMsg.parts.push({
              id: crypto.randomUUID(),
              messageID: firstUserMsg.info.id,
              sessionID: firstUserMsg.info.sessionID,
              type: "text",
              text: bootstrapText,
              synthetic: true,
            })
          }
        }

        // Fetch pending messages
        const msgResult = await $.nothrow()`hcom opencode-read --name ${instanceName}`.quiet()
        if (msgResult.exitCode !== 0) return
        let rawMessages: any[] = []
        try { rawMessages = JSON.parse(msgResult.text()) } catch { return }
        if (!Array.isArray(rawMessages) || rawMessages.length === 0) return

        const maxId = Math.max(...rawMessages.map((m: any) => m.event_id || 0))
        if (maxId === 0) return

        // Inject as synthetic part
        const formatted = formatMessagesForInjection(rawMessages, instanceName)
        const lastMsg = findLastUserMessage(output.messages)
        if (lastMsg) {
          lastMsg.parts.push({
            id: crypto.randomUUID(),
            messageID: lastMsg.info.id,
            sessionID: lastMsg.info.sessionID,
            type: "text",
            text: formatted,
            synthetic: true,
          })
          // Ack immediately with explicit max_id — sequential, no race.
          // Transform is awaited by OpenCode, so next transform won't fire
          // until this returns and the LLM completes its step.
          await $.nothrow()`hcom opencode-read --name ${instanceName} --ack --up-to ${String(maxId)}`.quiet()
          log("INFO", "plugin.messages_delivered", instanceName, {
            msg: `injected ${rawMessages.length} messages, acked to ${maxId}`,
            count: rawMessages.length,
            acked_to: maxId,
          })
        }
      } catch (e) {
        log("ERROR", "plugin.transform_error", instanceName, { error: String(e) })
      }
    },

    "experimental.session.compacting": async (input, output) => {
      try {
        if (!checkHcom()) return
        if (!instanceName) return

        output.context.push(
          `You are connected to hcom as "${instanceName}". ` +
          `Use --name ${instanceName} for all hcom commands.`
        )
        log("INFO", "plugin.compaction_reset", instanceName)
      } catch (e) {
        log("ERROR", "plugin.compaction_error", instanceName, { error: String(e) })
      }
    },
  }
}
