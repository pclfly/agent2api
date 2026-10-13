/** 提供商提示词行 → 保存载荷。纯函数，独立于 React store，便于回归验证。 */
export type PromptPatchSource = {
  mode: string
  file: string
  text: string
  source: string
}

/** 文件/内置正文只是展示值，不能顺手复制成 inline，否则文件热重载会失效。 */
export function providerPatch<T extends PromptPatchSource & { configured: boolean }>(
  prompt: PromptPatchSource,
  item: T,
): { promptMode: string; promptFile: string; promptText: string } {
  const current = item.configured ? item : prompt
  // source=none 时，后端 text 仅可能是透传模式下暂存的 inline 正文。
  const hasStoredText = current.source === 'inline' || current.source === 'none'
  return {
    promptMode: item.mode,
    promptFile: item.file,
    promptText: hasStoredText ? current.text : '',
  }
}
