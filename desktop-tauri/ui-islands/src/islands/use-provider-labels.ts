import * as React from 'react'
import { PROVIDER_LABELS_CHANGED } from './provider-label'

/** Repaint cached rows/options on directory arrival or rename, without fetching logs. */
export function useProviderLabels(): void {
  const [, redraw] = React.useReducer((version: number) => version + 1, 0)
  React.useEffect(() => {
    const onChange = () => redraw()
    window.addEventListener(PROVIDER_LABELS_CHANGED, onChange)
    // Close the render→effect race if providers.js completed in between.
    redraw()
    return () => window.removeEventListener(PROVIDER_LABELS_CHANGED, onChange)
  }, [])
}
