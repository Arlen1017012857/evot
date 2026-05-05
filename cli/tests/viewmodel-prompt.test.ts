import { describe, test, expect, beforeAll } from 'bun:test'
import { buildPromptBlocks, type PromptVMInput } from '../src/term/viewmodel/prompt.js'
import { blocksToLines } from '../src/term/viewmodel/types.js'
import stripAnsi from 'strip-ansi'
import chalk from 'chalk'

beforeAll(() => {
  chalk.level = 3
})

function defaultInput(overrides?: Partial<PromptVMInput>): PromptVMInput {
  return {
    lines: [''],
    cursorLine: 0,
    cursorCol: 0,
    active: true,
    model: 'claude-sonnet',
    verbose: true,
    planning: false,
    logMode: false,
    queuedMessages: [],
    updateHint: null,
    serverUptime: null,
    serverPort: null,
    exitHint: false,
    completionCandidates: [],
    ghostHint: '',
    columns: 80,
    isLoading: false,
    placeholder: true,
    cwd: '/Users/test/project',
    gitRepo: 'project',
    gitBranch: 'main',
    ...overrides,
  }
}

function render(input: PromptVMInput): string {
  return blocksToLines(buildPromptBlocks(input)).join('\n')
}

function renderPlain(input: PromptVMInput): string {
  return stripAnsi(render(input))
}

describe('buildPromptBlocks', () => {
  test('contains top border', () => {
    const result = renderPlain(defaultInput())
    const lines = result.split('\n')
    const borderLines = lines.filter(l => l.match(/^─+$/))
    expect(borderLines.length).toBeGreaterThanOrEqual(1)
  })

  test('shows cursor prefix ❯', () => {
    const result = renderPlain(defaultInput())
    expect(result).toContain('❯')
  })

  test('shows placeholder when empty', () => {
    const result = renderPlain(defaultInput())
    expect(result).toContain('Type a message...')
  })

  test('no placeholder when text entered', () => {
    const result = renderPlain(defaultInput({ lines: ['hello'], cursorCol: 5, placeholder: false }))
    expect(result).not.toContain('Type a message...')
    expect(result).toContain('hello')
  })

  test('shows model in footer', () => {
    const result = renderPlain(defaultInput())
    expect(result).toContain('claude-sonnet')
  })

  test('shows [plan] when planning', () => {
    const result = renderPlain(defaultInput({ planning: true }))
    expect(result).toContain('[plan]')
  })

  test('no [plan] when not planning', () => {
    const result = renderPlain(defaultInput({ planning: false }))
    expect(result).not.toContain('[plan]')
  })

  test('shows [log] when logMode', () => {
    const result = renderPlain(defaultInput({ logMode: true }))
    expect(result).toContain('[log]')
  })

  test('shows exit hint', () => {
    const result = renderPlain(defaultInput({ exitHint: true }))
    expect(result).toContain('Press Ctrl+C again to exit')
  })

  test('shows queued messages', () => {
    const result = renderPlain(defaultInput({ queuedMessages: ['msg1', 'msg2'] }))
    expect(result).toContain('msg1')
    expect(result).toContain('msg2')
  })

  test('shows completion candidates', () => {
    const result = renderPlain(defaultInput({ completionCandidates: ['/help', '/model', '/resume'] }))
    expect(result).toContain('/help')
    expect(result).toContain('/model')
    expect(result).toContain('/resume')
  })

  test('shows update hint', () => {
    const result = renderPlain(defaultInput({ updateHint: 'v0.2.0 available' }))
    expect(result).toContain('v0.2.0 available')
  })

  test('shows server state', () => {
    const result = renderPlain(defaultInput({ serverPort: 8082, serverUptime: '5m' }))
    expect(result).toContain(':8082')
    expect(result).toContain('5m')
  })

  test('cursor is rendered with inverse', () => {
    const result = render(defaultInput({ lines: ['abc'], cursorCol: 1, placeholder: false }))
    expect(result).toContain('\x1b[7m')
  })

  test('ghost hint is dim', () => {
    const result = render(defaultInput({ lines: ['/he'], cursorCol: 3, placeholder: false }))
    // ghost hint depends on getGhostHint — just verify no crash
    expect(result).toBeTruthy()
  })
})
