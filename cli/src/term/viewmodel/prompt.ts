import { line, block, plain, dim, bold, colored, inverse, type ViewBlock, type StyledLine, type StyledSpan } from './types.js'

export interface PromptVMInput {
  lines: string[]
  cursorLine: number
  cursorCol: number
  active: boolean
  model: string
  verbose: boolean
  planning: boolean
  logMode: boolean
  queuedMessages: string[]
  updateHint: string | null
  serverUptime: string | null
  serverPort: number | null
  exitHint: boolean
  completionCandidates: string[]
  ghostHint: string
  columns: number
  isLoading: boolean
  placeholder: boolean
  cwd: string
  gitRepo: string | null
  gitBranch: string | null
}

export function buildPromptBlocks(input: PromptVMInput): ViewBlock[] {
  const blocks: ViewBlock[] = []
  const border = '─'.repeat(input.columns)

  blocks.push(block([line(dim(border))], 1))

  const inputLines: StyledLine[] = []
  for (let i = 0; i < input.lines.length; i++) {
    const text = input.lines[i]!
    const prefix: StyledSpan = i === 0
      ? colored('❯ ', 'cyan', { bold: true })
      : plain('  ')

    if (i === input.cursorLine && input.active) {
      if (text === '' && input.lines.length === 1 && input.placeholder) {
        inputLines.push(line(prefix, inverse(' '), dim(' Type a message...')))
      } else {
        const before = text.slice(0, input.cursorCol)
        const cursorChar = text[input.cursorCol] ?? ' '
        const after = text.slice(input.cursorCol + 1)
        const spans: StyledSpan[] = [prefix, plain(before), inverse(cursorChar), plain(after)]
        if (input.ghostHint) spans.push(dim(input.ghostHint))
        inputLines.push(line(...spans))
      }
    } else {
      inputLines.push(line(prefix, plain(text || ' ')))
    }
  }
  blocks.push(block(inputLines))

  if (input.completionCandidates.length > 1) {
    blocks.push(block([line(dim('  ' + input.completionCandidates.join('  ')))]))
  }

  blocks.push(block([line(dim(border))]))

  if (input.exitHint) {
    blocks.push(block([line(dim('  Press Ctrl+C again to exit'))]))
  }

  if (input.queuedMessages.length > 0) {
    const qLines: StyledLine[] = input.queuedMessages.map(msg =>
      line(dim('  ❯ '), dim(msg))
    )
    blocks.push(block(qLines))
  }

  const footerBlocks = buildFooter(input)
  blocks.push(footerBlocks)

  return blocks
}

function buildFooter(input: PromptVMInput): ViewBlock {
  const leftSpans: StyledSpan[] = []

  // model
  leftSpans.push(dim(input.model))

  // [log] / [plan] modes
  if (input.logMode) {
    leftSpans.push(plain('  '))
    leftSpans.push(colored('[log]', 'magenta', { bold: true }))
    leftSpans.push(dim(' Esc to exit'))
  }
  if (input.planning) {
    leftSpans.push(plain('  '))
    leftSpans.push(colored('[plan]', 'yellow', { bold: true }))
  }

  // git repo · branch
  if (input.gitRepo) {
    leftSpans.push(dim(' · '))
    let cwd = input.cwd
    const home = process.env.HOME || process.env.USERPROFILE || ''
    if (home && cwd.startsWith(home)) {
      cwd = '~' + cwd.slice(home.length)
    }
    leftSpans.push(dim(cwd))
    if (input.gitBranch) {
      leftSpans.push(dim(' (' + input.gitBranch + ')'))
    }
  }

  const rightSpans: StyledSpan[] = []
  if (input.serverPort != null && input.serverUptime) {
    rightSpans.push(colored(`[server :${input.serverPort} · ${input.serverUptime}]`, 'green'))
  }
  if (input.updateHint) {
    if (rightSpans.length > 0) rightSpans.push(plain('  '))
    rightSpans.push(colored(input.updateHint, 'yellow'))
  }

  const leftText = leftSpans.map(s => s.text).join('')
  const rightText = rightSpans.map(s => s.text).join('')
  const gap = Math.max(1, input.columns - leftText.length - rightText.length)

  const spans: StyledSpan[] = [...leftSpans, plain(' '.repeat(gap)), ...rightSpans]
  return block([line(...spans)])
}
