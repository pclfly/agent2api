/** null=全部（含新增），[]=全不选；显式范围保存不随目录刷新扩大。 */
export type PromptModelScope = string[] | null

export function readPromptModelScope(raw: unknown): PromptModelScope {
  if (raw == null) return null
  if (!Array.isArray(raw)) return []
  const result: string[] = []
  for (const item of raw) {
    if (typeof item !== 'string' || !item.trim()) return []
    const id = item.trim()
    if (!result.some(known => known.toLowerCase() === id.toLowerCase())) result.push(id)
  }
  return result
}

export function promptModelOptions(catalog: string[], selected: PromptModelScope): string[] {
  const unique = new Map<string, string>()
  for (const id of [...catalog, ...(selected ?? [])]) {
    if (id.trim() && !unique.has(id.toLowerCase())) unique.set(id.toLowerCase(), id)
  }
  return [...unique.values()]
}

export function isPromptModelSelected(scope: PromptModelScope, id: string): boolean {
  return scope === null || scope.some(value => value.toLowerCase() === id.toLowerCase())
}

export function togglePromptModel(scope: PromptModelScope, catalog: string[], id: string, checked: boolean): PromptModelScope {
  const selected = scope === null ? promptModelOptions(catalog, null) : [...scope]
  const next = selected.filter(value => value.toLowerCase() !== id.toLowerCase())
  if (checked) next.push(id)
  // 不自动变null：勾满当前目录不等于允许以后新增的模型。
  return next
}
