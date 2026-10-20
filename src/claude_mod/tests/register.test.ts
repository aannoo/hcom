import { expect, test } from 'claude-code/testing'
import { register } from '../hooks/register.js'

function harness() {
  const handlers = {} as Record<string, Function>
  let tick: Function
  let cancelled = false
  const prompts = [] as any[]
  let response = { wake: true }
  let resolvePrompt: Function
  const api = {
    env: { get: async (key: string) => key === 'HCOM_PROCESS_ID' ? 'process-1' : key === 'HCOM_PTY_MODE' ? '1' : key === 'HCOM_CLAUDE_DELIVERY' ? 'native' : '["hcom"]' },
    clock: { every: (_: number, fn: Function) => { tick = fn; return { cancel: () => { cancelled = true } } } },
    session: { id: async () => 'session-1' },
    process: { run: async () => ({ exitCode: 0, stdout: JSON.stringify(response) }) },
    prompt: { submit: (value: any) => { prompts.push(value); return new Promise(resolve => { resolvePrompt = resolve }) } },
    // Claude's $.ui.log is synchronous and returns undefined.
    ui: { log: (_: string) => {} },
  }
  register((event: string, fn: Function) => { handlers[event] = fn })
  return {
    api, handlers, prompts,
    tick: () => tick(api),
    cancelled: () => cancelled,
    response: (value: any) => { response = value },
    finishPrompt: () => resolvePrompt({ text: '<hcom>', asUser: true }),
  }
}
const next = async (event: any) => event
const interactive = { isInteractive: true, surface: 'terminal', cwd: '/work' }

test('one pending submission while the bridge keeps polling', async () => {
  const h = harness()
  await h.handlers['session.start'](h.api, interactive, next)
  await h.tick()
  await h.tick()
  expect(h.prompts.length).toBe(1)
  expect(h.prompts[0]).toEqual({ text: '<hcom>', asUser: true })
  h.finishPrompt()
  await Promise.resolve()
  h.response({ wake: false })
  await h.tick()
  expect(h.prompts.length).toBe(1)
})

test('an old poll cannot submit after a session switch', async () => {
  const h = harness()
  let finish: Function
  h.api.process.run = () => new Promise(resolve => { finish = resolve }) as any
  await h.handlers['session.start'](h.api, interactive, next)
  const pending = h.tick()
  for (let i = 0; i < 5; i++) await Promise.resolve()
  await h.handlers['session.end'](h.api, { reason: 'resume' }, next)
  finish!({ exitCode: 0, stdout: '{"wake":true}' })
  await pending
  expect(h.prompts.length).toBe(0)
  expect(h.cancelled()).toBe(false)
})

test('headless sessions do not start polling', async () => {
  const h = harness()
  await h.handlers['session.start'](h.api, { ...interactive, isInteractive: false }, next)
  expect(h.prompts.length).toBe(0)
})

test('session exit cancels polling', async () => {
  const h = harness()
  await h.handlers['session.start'](h.api, interactive, next)
  await h.handlers['session.end'](h.api, { reason: 'prompt_input_exit' }, next)
  expect(h.cancelled()).toBe(true)
})

test('a queued mod wake cannot enter a different session', async () => {
  const h = harness()
  await h.handlers['session.start'](h.api, interactive, next)
  await h.tick()
  h.api.session.id = async () => 'session-2'
  const result = await h.handlers['prompt.submit'](h.api, {
    text: '<hcom>', origin: { kind: 'plugin', name: 'hcom' },
  }, next)
  expect(result.drop).toBe('hcom wake belongs to a previous session')
  h.finishPrompt()
})

test('failed submission is reported without switching transport', async () => {
  const h = harness()
  const reports = [] as string[][]
  h.api.prompt.submit = () => Promise.reject(new Error('submission denied'))
  h.api.process.run = async (argv: string[]) => {
    if (argv.includes('--event')) reports.push(argv)
    return { exitCode: 0, stdout: '{"wake":true}' }
  }
  await h.handlers['session.start'](h.api, interactive, next)
  await h.tick()
  for (let i = 0; i < 20; i++) await Promise.resolve()
  expect(h.cancelled()).toBe(false)
  expect(reports.length).toBe(1)
  expect(reports[0].includes('submission-failed')).toBe(true)
  expect(reports[0].includes('Error: submission denied')).toBe(true)
})

test('disabled experimental mode does not start polling', async () => {
  const h = harness()
  const get = h.api.env.get
  h.api.env.get = async (key: string) => key === 'HCOM_CLAUDE_DELIVERY' ? 'inject' : get(key)
  await h.handlers['session.start'](h.api, interactive, next)
  expect(h.prompts.length).toBe(0)
})

test('polling observes without claiming while a submission is pending', async () => {
  const h = harness()
  const calls = [] as string[][]
  h.api.process.run = async (argv: string[]) => {
    calls.push(argv)
    return { exitCode: 0, stdout: '{"wake":true}' }
  }
  await h.handlers['session.start'](h.api, interactive, next)
  await h.tick()
  await h.tick()
  expect(calls[0].includes('--observe')).toBe(false)
  expect(calls[1].includes('--observe')).toBe(true)
  expect(h.prompts.length).toBe(1)
  h.finishPrompt()
})

test('invalid mod command reports startup failure without blocking the hook', async () => {
  for (const value of [undefined, '{broken', '[]', '"hcom"', '[1]']) {
    const h = harness()
    const logs = [] as string[]
    const get = h.api.env.get
    h.api.env.get = async (key: string) => key === 'HCOM_MOD_COMMAND' ? value as any : get(key)
    h.api.ui.log = (text: string) => { logs.push(text) }
    const result = await h.handlers['session.start'](h.api, interactive, next)
    expect(result).toEqual(interactive)
    expect(logs.length).toBe(1)
    expect(logs[0].includes('invalid HCOM_MOD_COMMAND')).toBe(true)
    expect(h.prompts.length).toBe(0)
  }
})

test('a dropped wake is reported as a failure, not a submission', async () => {
  const h = harness()
  const reports = [] as string[][]
  h.api.prompt.submit = () => Promise.resolve({ drop: 'blocked by another plugin' }) as any
  h.api.process.run = async (argv: string[]) => {
    if (argv.includes('--event')) reports.push(argv)
    return { exitCode: 0, stdout: '{"wake":true}' }
  }
  await h.handlers['session.start'](h.api, interactive, next)
  await h.tick()
  for (let i = 0; i < 20; i++) await Promise.resolve()
  expect(reports.length).toBe(1)
  expect(reports[0].includes('submission-failed')).toBe(true)
  expect(reports[0].includes('wake dropped: blocked by another plugin')).toBe(true)
})

test('polls keep observing until a finished wake is reported', async () => {
  const h = harness()
  const calls = [] as string[][]
  let finishReport: Function
  h.api.process.run = (argv: string[]) => {
    calls.push(argv)
    if (argv.includes('--event')) return new Promise(resolve => { finishReport = resolve }) as any
    return Promise.resolve({ exitCode: 0, stdout: '{"wake":true}' }) as any
  }
  await h.handlers['session.start'](h.api, interactive, next)
  await h.tick()
  h.finishPrompt()
  for (let i = 0; i < 5; i++) await Promise.resolve()
  // The outcome report is still running: a poll now must not claim a wake.
  await h.tick()
  expect(calls.at(-1)!.includes('--observe')).toBe(true)
  expect(h.prompts.length).toBe(1)
  finishReport!({ exitCode: 0, stdout: '{"wake":false}' })
  for (let i = 0; i < 5; i++) await Promise.resolve()
  await h.tick()
  expect(calls.at(-1)!.includes('--observe')).toBe(false)
  expect(h.prompts.length).toBe(2)
  h.finishPrompt()
})

test('a failed poll backs off instead of respawning every tick', async () => {
  const h = harness()
  let polls = 0
  h.api.process.run = async (argv: string[]) => {
    if (!argv.includes('--event')) polls++
    return { exitCode: 1, stdout: '', stderr: 'database is locked' }
  }
  await h.handlers['session.start'](h.api, interactive, next)
  for (let i = 0; i < 21; i++) await h.tick()
  expect(polls).toBe(1)
  await h.tick()
  expect(polls).toBe(2)
})

// One failure is a tick plus the 20 backoff ticks that follow it.
async function fail(h: ReturnType<typeof harness>, n: number) {
  for (let i = 0; i < n * 21; i++) await h.tick()
}

test('an isolated failed poll, as after host sleep, stays silent', async () => {
  const h = harness()
  const reports = [] as string[][]
  const logs = [] as string[]
  let ok = false
  h.api.ui.log = (text: string) => { logs.push(text) }
  h.api.process.run = async (argv: string[]) => {
    if (argv.includes('--event')) { reports.push(argv); return { exitCode: 0, stdout: '{"wake":false}' } }
    return ok ? { exitCode: 0, stdout: '{"wake":false}' } : { exitCode: 1, stdout: '', stderr: 'aborted: still running after 30000ms' }
  }
  await h.handlers['session.start'](h.api, interactive, next)
  for (let round = 0; round < 3; round++) {
    ok = false
    await fail(h, 2)
    ok = true
    await h.tick()
  }
  expect(reports.length).toBe(0)
  expect(logs.length).toBe(0)
})

test('repeated failed polls are reported every time and logged once', async () => {
  const h = harness()
  const reports = [] as string[][]
  const logs = [] as string[]
  let ok = false
  h.api.ui.log = (text: string) => { logs.push(text) }
  h.api.process.run = async (argv: string[]) => {
    if (argv.includes('--event')) { reports.push(argv); return { exitCode: 0, stdout: '{"wake":false}' } }
    return ok ? { exitCode: 0, stdout: '{"wake":false}' } : { exitCode: 1, stdout: '', stderr: 'notify bind failed' }
  }
  await h.handlers['session.start'](h.api, interactive, next)
  await fail(h, 4)
  // hcom clears the error after one good cycle, so each repeat must set it again.
  expect(reports.length).toBe(2)
  expect(reports.every(r => r.includes('poll-failed'))).toBe(true)
  expect(logs.length).toBe(1)
  expect(logs[0].includes('notify bind failed')).toBe(true)
  ok = true
  await h.tick()
  ok = false
  await fail(h, 3)
  expect(reports.length).toBe(3)
  expect(logs.length).toBe(2)
})

test('hcom runs outside the session directory, which a Bash command may delete', async () => {
  const h = harness()
  const inits = [] as any[]
  h.api.process.run = async (argv: string[], init?: any) => {
    inits.push(init)
    if (argv.includes('--event')) return { exitCode: 0, stdout: '{}' }
    return { exitCode: 0, stdout: '{"wake":true}' }
  }
  h.api.prompt.submit = () => Promise.resolve(undefined) as any
  await h.handlers['session.start'](h.api, interactive, next)
  await h.tick()
  for (let i = 0; i < 20; i++) await Promise.resolve()
  expect(inits.length).toBe(2)
  expect(inits.every(init => init.cwd === '/')).toBe(true)
})

test('a poll that cannot record its failure still logs and backs off', async () => {
  const h = harness()
  const logs = [] as string[]
  let polls = 0
  h.api.ui.log = (text: string) => { logs.push(text) }
  h.api.process.run = async (argv: string[]) => {
    if (!argv.includes('--event')) polls++
    throw new Error('failed to start: ENOENT')
  }
  await h.handlers['session.start'](h.api, interactive, next)
  await fail(h, 3)
  expect(polls).toBe(3)
  expect(logs.length).toBe(2)
  expect(logs[0].includes('could not record poll-failed')).toBe(true)
  expect(logs[1].includes('poll failed; messages remain queued')).toBe(true)
})
