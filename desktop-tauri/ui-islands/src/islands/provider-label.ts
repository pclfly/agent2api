/** Shared display-only provider resolution. Never use a label as a filter/query ID. */
export const PROVIDER_LABELS_CHANGED = 'wb:provider-labels-changed'

export function resolveProviderLabel(
  provider: unknown,
  backendLabel?: unknown,
  lookup?: (id: string) => string | undefined,
): string {
  const id = String(provider ?? '').trim()
  if (!id) return ''
  const current = String(lookup?.(id) ?? '').trim()
  // The live directory wins on rename; an unresolved directory echoes the ID.
  if (current && current !== id) return current
  const fallback = String(backendLabel ?? '').trim()
  return fallback && fallback !== id ? fallback : id
}

export function providerOptionsForDisplay(
  options: readonly { value: string; label: string }[],
  selected: string,
  lookup?: (id: string) => string | undefined,
): { value: string; label: string }[] {
  const withSelected = !selected || options.some(item => item.value === selected)
    ? options
    : [{ value: selected, label: selected }, ...options]
  return withSelected.map(item => ({
    value: item.value,
    label: resolveProviderLabel(item.value, item.label, lookup),
  }))
}
