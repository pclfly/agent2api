import type { PromptState, ProviderPromptState } from './settings-state'

export function providerPromptRows(prompt: PromptState): ProviderPromptState[] {
  const configured = new Map(prompt.providers.map(item => [item.id, item]))
  const ids = new Set([
    ...configured.keys(),
    ...Object.keys(prompt.gateway),
    ...Object.keys(prompt.gatewayText),
  ])
  const ordered = [
    ...prompt.options.map(item => item.id).filter(id => ids.has(id)),
    ...ids,
  ]
  const seen = new Set<string>()
  const rows: ProviderPromptState[] = []
  for (const id of ordered) {
    if (seen.has(id)) continue
    seen.add(id)
    const item = configured.get(id)
    rows.push(item ? {
      ...item,
      gateway: prompt.gateway[id] ?? true,
      configured: true,
    } : {
      id,
      // 补出来的行没有自己的范围（它本来就没配过这一家）—— `null` = 全部，
      // 与「没配过覆盖」的语义一致：一旦用户给它配范围，那一行就变成
      // `configured: true` 的真实覆盖
      models: null,
      mode: prompt.mode,
      file: prompt.file,
      text: prompt.text,
      source: prompt.source,
      lines: prompt.lines,
      fileError: prompt.fileError,
      gateway: prompt.gateway[id] ?? true,
      configured: false,
    })
  }
  return rows
}
